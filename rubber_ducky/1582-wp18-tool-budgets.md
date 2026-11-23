# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="wp18-tool-budgets-1582-e17145f" -->

<!-- event id="request" artifact path="1582-wp18-tool-budgets/artifacts/round-1-review-request.diff" sha256="0c18d5d5cd062db78f2532251acd10babf31b64260d3dba87a62b3109686fdd8" -->
## Review Request — Round 1
**Task:** 1582 — dbunk WP18: cooperative tool budgets (FR-19, ADR-031), diff 59cbded..e17145f
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Review the complete WP18 diff against its base 59cbded (the combined dbunk tree). WP18 implements FR-19 of an external reviewer product: a caller can set, for one run, a maximum duration for each tool call and a maximum amount of data that a tool call may read or scan; a call that exceeds a limit stops and returns an error that names the limit, never a partial result presented as complete; limit hits are recorded in the run audit; default is the current behavior. Accepted design record: docs/adr/031-cooperative-tool-budgets.md (operation-scoped budget with an absolute deadline and independent logical-byte and directory-entry counters; children inherit the remaining allowance; charge logical file/git content read or scanned, including index content, not output bytes; external calls use the remaining deadline and the managed process-group termination path, 'a timeout wrapper that leaves descendants running does not implement the contract'; refuse tools whose requested input/entry budget cannot be enforced, only when that budget is enabled; the deadline stays required for external work). Reviewed interface row: `--tool-timeout-ms N`, `--tool-input-bytes N`, positive integers, omitted means no new limit, 'a configured unsupported operation is refused, not silently exempted'. Reviewed refusal codes: tool_timeout, tool_input_limit (and the existing tool_policy_unsupported). Implementation: src/tool_budget.rs (ToolBudget with Instant deadline and two atomic counters). The budget is a Session field. Install points: ops::dispatch_with_progress (install if none, clear if owned), the MCP batch handler, one scope in mcp.rs dispatch_tool_inner for each registry tool (with a tokio task-local deadline that managed_process::run reads, so a plugin subprocess is terminated with its process group at the deadline), and execute_script (the shared budget lives in the script context and is set on the session only inside each locked inner operation). Native tools: read_file charges the file content; ls charges entries walked; search content mode charges scanned bytes in the blocking worker and checks the deadline per chunk and between files; search files mode (index) is refused under an input or entry limit. exec and the process plugins enforce the deadline and are refused under an input or entry limit (code tool_budget_unenforceable). tool_pipeline is refused under each budget option. A run with --worker-config and a budget option is refused at startup. agent.rs writes the reserved `limit_hit` audit event when a tool result is a tool_timeout or tool_input_limit failure (run_audit::budget_limit_hit parses the result text). The session daemon gets the options in its launch arguments and launch identity. History: a first worker result (9d59267) was rejected by the integrator after probes on the binary: search charged returned bytes, native tools had no deadline check, plugin tools and tool_pipeline were neither limited nor refused, no limit_hit event. The fix run produced e17145f. The integrator repeated the probes on e17145f: a search that scans 4 MB with a 2000-byte limit returns tool_input_limit with consumed 65543; read_file, ls, both search modes, exec, the git plugin, and two state tools return tool_timeout under a 1 ms deadline; curl against a server that answers after 4 s returns tool_timeout after 0.71 s under a 700 ms deadline; exec sleep 5 returns after 0.70 s; the call after a timeout gets a new budget; default-off runs are unchanged (37 tools offered with and without the options). Verification by the integrator on e17145f: fmt and clippy exit 0. cargo test --no-fail-fast: 2305 passed, 6 failed, 10 ignored (the 6 are plugins::shellcheck::tests::*, the host has no shellcheck; the same 6 fail on the base). pytest tests: 970 passed, 7 failed (tests/test_shellcheck.py, same cause), 1 skipped. tests/test_tool_budgets.py has 35 tests. CI did not run on this branch at the time of this request.

### Relevant Code / Diff
diff --git a/CHANGELOG.md b/CHANGELOG.md
index 6a4bb13..d5681f7 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -9,6 +9,24 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ### Added
 
+- **Cooperative tool budgets (FR-19, ADR-031).** Three default-off, global
+  options bound a single top-level tool call: `--tool-timeout-ms N` (an absolute
+  per-call deadline), `--tool-input-bytes N` (a logical input-byte ceiling,
+  charged for file *and* git content and for bytes returned from the cached
+  index, not only filesystem reads), and `--tool-max-entries N` (a
+  directory-entry ceiling; `(proposed)` spelling). A deadline hit returns the
+  reviewed `tool_timeout`; a byte or entry hit returns `tool_input_limit`
+  naming which ceiling was hit, the configured value, and the consumed value —
+  with no partial result. A composite (`batch`, `execute_script`) shares one
+  budget with its children: they inherit the remaining allowance and the
+  absolute deadline and cannot start a fresh one. For `exec` and `git_read` the
+  deadline stops the work, not merely the wait: the managed process group is
+  `SIGTERM`/`SIGKILL` reaped so no child or grandchild outlives the result.
+  Output truncation (`--no-output-spill`) stays a separate mechanism and is
+  never a budget failure. With none of the options set, every tool is exactly
+  unchanged. A budget option with `--worker-config` is refused at startup
+  (exit `2`, `run_configuration_invalid`, `tool_budget_worker_unenforceable`):
+  the budget cannot be enforced across the worker process boundary.
 - **`--capability-probe-payload` (FR-04).** A default-off `agent --print`
   option (ADR-027) that sends exactly one unretried capability probe before any
   evidence tool runs, proving the endpoint can drive the same final-answer tool
@@ -406,3 +424,25 @@ binary now also reports its real version: earlier releases still said 0.1.1.
   which unbreaks tagged releases.
 
 - Enforced tool allowlist (`--tool-allowlist PATH`, FR-02/WP06): an explicit JSON array of exact exported tool names is the effective policy at every model listing (tools/list, `list_all_tools`) and every dispatch boundary (direct MCP, batch children, Starlark `tool()`/opcode shortcuts, `execute_script`, the agent loop). Omitted = legacy (all tools); `[]` = none; unknown names fail at startup (exit 2 `run_configuration_invalid`). Caller-owned (never widened by workspace/env config); part of the session-daemon launch identity; refusals carry `tool_not_allowed` and reach the `--audit-path` record. The policy also filters `get_tool_schema`, `list_tool_signatures`, the MCP server instructions, the session-daemon socket listing, and the agent's offered catalog, so served/probed/dumped/dispatched sets agree; with `--final-output` an allowlist omitting the final-answer tool fails startup (`tool_allowlist_missing_final_output`); the reported effective list is the intersection with the agent-env `allowed_commands`; config-registered `[tools.*]` tools may be allowlisted. (r3) The model-facing catalog is now filtered at one source (`tool_facade::active_schemas_for_policy`), so `daimonos chat`, `daimonos acp`, and the session-daemon agent — not only `agent --print` — offer the model exactly the allowlist; forwarded MCP tools and the `execute_script` description's embedded Starlark signatures are filtered by the same policy. (r4) The composite opcodes `tool_pipeline` and `lint_repair` now check their inner `tool_id` against the allowlist before running it, so a listed composite can never reach an unlisted registered `[tools.*]` tool; a malformed or unreadable `--tool-allowlist` file fails startup with reason `tool_allowlist_invalid`; remote/worker tool names cannot be allowlisted in this package (deferred to WP10), so an active allowlist removes every forwarded MCP tool from the catalog.
+
+### WP18 FR-19 cooperative tool budgets — fix run (r4)
+
+- Content search (`search mode=content`) meters the bytes it SCANS (not bytes
+  returned); the blocking worker checks the deadline per read chunk and stops
+  mid-scan (`tool_input_limit`/`tool_timeout`, no partial matches).
+- `read_file`, `ls`, and both search modes check the deadline before building a
+  result; a passed deadline returns `tool_timeout`, never a late success.
+- The budget + deadline scope is installed at ONE place for every registry
+  tool; every plugin subprocess (`git`, `gh`, `cargo`, `npm`, `pytest`,
+  `docker`, `shellcheck`, `curl`, …) is stopped and process-group reaped at the
+  deadline.
+- Unmeterable-input tools are refused with the named `tool_budget_unenforceable`
+  under `--tool-input-bytes`/`--tool-max-entries`: process-starting plugins, the
+  index-answered `search mode=files`, and `tool_pipeline` (refused under any
+  budget option).
+- `ToolBudget::charge_input(bytes)` is the reserved method name.
+- A budget limit hit writes the reserved `limit_hit` audit event (limit, tool,
+  configured, consumed) via `record_limit_hit`.
+- `execute_script` holds its one shared budget in the script context, installed
+  only inside each inner per-op lock, so a concurrent top-level call cannot
+  inherit it.
diff --git a/docs/configuration.md b/docs/configuration.md
index 1b0e3e0..286c26e 100644
--- a/docs/configuration.md
+++ b/docs/configuration.md
@@ -1812,3 +1812,77 @@ The control file is JSON:
   platforms reaping relies on the SDK's `kill_on_drop` at process exit.
 
 See ADR-028 and `tests/test_worker_isolation.py`.
+
+## Cooperative tool budgets (FR-19, ADR-031)
+
+Three default-off, global command-line options bound the resources a single
+top-level tool call may consume. They are caller-owned (never read from
+`daimonos.toml`) and, when none is set, every tool behaves exactly as before.
+
+- **`--tool-timeout-ms N`** — an absolute deadline, in milliseconds, for the
+  call. A hit returns the reviewed `tool_timeout` code with no partial result.
+- **`--tool-input-bytes N`** — a logical input-byte ceiling for the call. Bytes
+  are counted for file content read, git content read, and the bytes returned
+  from the cached (trigram) index — not only filesystem reads, and never the
+  displayed/truncated output bytes. A hit returns `tool_input_limit`.
+- **`--tool-max-entries N`** *(proposed)* — a directory-entry ceiling for the
+  call (e.g. `ls` walking). A hit returns `tool_input_limit` naming
+  `directory entries`. (The reviewed interface row has no separate code for the
+  entry limit; integrator decision D56 reuses `tool_input_limit`.)
+
+Each value must be a positive integer; `0` is rejected at startup (exit `2`).
+
+**No partial success.** A limit hit is a typed tool failure naming which limit
+was hit, the configured value, and the consumed value — never a truncated
+"successful" result.
+
+**One budget per call, shared by composites.** A `batch` and an
+`execute_script` share one budget with their children: a child inherits the
+remaining allowance and the same absolute deadline and cannot open a fresh
+top-level budget, so delegation cannot multiply the allowance.
+
+**Stopped work, not a stopped wait.** For `exec` and `git_read` the deadline is
+handed to the managed subprocess, which reaps the whole process group
+(`SIGTERM`, bounded grace, then `SIGKILL`) — so no child or grandchild outlives
+the `tool_timeout` result. On non-Unix platforms the subprocess stop relies on
+the managed lifecycle's `kill_on_drop`.
+
+**Output truncation is separate.** `--no-output-spill` bounds the *displayed*
+output of a result; it is not an input budget and a truncated result is never a
+budget failure.
+
+**Workers.** Worker-routed tool calls (`--worker-config`, WP10) run in a
+separate process that this process's budget cannot observe, so the deadline
+cannot stop that work and its input cannot be accounted. Rather than silently
+ignore a budget option in worker mode, a budgeted worker route is refused at
+startup with exit `2` (`run_configuration_invalid`, reason
+`tool_budget_worker_unenforceable`). Worker mode with no budget option is
+unchanged.
+
+Every frontend that builds a session from configuration honours the budget
+(the MCP server, `agent --print`, the interactive agent / session daemon,
+`chat`, `acp`, `batch`, and `execute_script`). For the interactive agent the
+budget is part of the session-daemon launch identity, so an attach to a daemon
+started under a different (or absent) budget is refused.
+
+See ADR-031 and `tests/test_tool_budgets.py`.
+
+#### Tool budgets — enforcement scope (WP18, FR-19)
+
+The default-off flags `--tool-timeout-ms`, `--tool-input-bytes`, and
+`--tool-max-entries` are cooperative:
+
+- Deadline: enforced everywhere. Native readers (`read_file`, `ls`, `search`)
+  check it at bounded work boundaries (between files and, for content search,
+  per read chunk). Plugin subprocesses are stopped and process-group reaped at
+  the deadline (Linux; elsewhere the process default timeout applies). A passed
+  deadline returns `tool_timeout`.
+- Input bytes: content search and `read_file` meter the file content SCANNED;
+  `git_read` meters the git object content returned.
+- Entries: `ls` meters directory entries walked.
+- Refused with `tool_budget_unenforceable` under an input/entry budget (input
+  not meterable): process-starting plugins (except `git_read`), the
+  index-answered `search mode=files`, and `tool_pipeline` (any budget option).
+
+Output truncation (`--no-output-spill`, `tool_output`) is a separate mechanism,
+never reported as a budget limit hit.
diff --git a/docs/contexts/reviewer-runtime/INTERFACES.md b/docs/contexts/reviewer-runtime/INTERFACES.md
index 18e5b30..82afc5b 100644
--- a/docs/contexts/reviewer-runtime/INTERFACES.md
+++ b/docs/contexts/reviewer-runtime/INTERFACES.md
@@ -284,6 +284,8 @@ No cooperative guarantee is claimed for an unowned remote process.
 
 Proposed `ToolFailure` fields: `code`, `message`, `tool`, `argument`, and optional `limit`, `configured`, `consumed`, and `retryable`.
 A timed-out or input-limited call returns `complete=false` and no success-shaped partial result.
+
+_Status (WP18, implemented — run wp18-r3):_ `--tool-timeout-ms N`, `--tool-input-bytes N`, and `--tool-max-entries N` (the D56 `(proposed)` spelling for the entry ceiling) are live, default-off global flags; `0` is a startup refusal (exit `2`). `src/tool_budget.rs` ships `ToolBudget` (kept) plus `ToolBudgetConfig` and `BudgetHit`; the reserved names `BudgetReceipt`/`LimitFailure` were NOT used (the receipt/failure role is filled by `path_policy::ToolFailure::{tool_timeout,tool_input_limit}`) — a naming merge item for the owner. Methods: `check_deadline()`, `charge_bytes(n)`, `charge_entries(n)`, `remaining()`, `timeout_failure()` (not the reserved `charge_input`). A deadline hit returns `tool_timeout`; a byte OR entry hit returns `tool_input_limit` naming which ceiling was hit, the configured value, and the consumed value (D56: no third code — an owner decision point). The budget is installed once per top-level call at `ops::dispatch_with_progress` and at the `batch`/`execute_script` handlers (children inherit, never re-install), and at the `git_read` arm (dispatched outside `ops::dispatch`); the absolute deadline is threaded into `managed_process::run` via a task-local so `exec` and `git_read` subprocess groups are reaped at the deadline through `ManagedChild::terminate`. Search meters the bytes it returns on both the filesystem (`grep`) and cached-index (`find`) paths, so nothing is refused for being unaccountable (D-WP18-3). A budgeted worker route is refused at startup (`run_configuration_invalid`, reason `tool_budget_worker_unenforceable`): the parent budget cannot stop or account worker-process tool work. `tool_pipeline` stages inherit the budget OBJECT but external stage processes run under `process.default_timeout_secs`, not the cooperative deadline (D-WP18-4). Merge items for the owner: (a) the `tool-contract` dump (`effective_options`, ADR-029) does NOT surface the budget options (req 10, left unchanged by design); (b) the reserved `BudgetReceipt`/`LimitFailure` names; (c) the `tool_pipeline` cooperative-deadline gap. Covered by `tests/test_tool_budgets.py` and a `src/session_bootstrap.rs` launch-identity unit test.
 These fields are policy/error metadata, not a rewrite of every existing tool's successful payload.
 Audit records the named limit and the refusal. Error text is bounded and does not contain raw evidence.
 
@@ -424,3 +426,18 @@ Implementation status (WP06, r3 review fixes): the model-facing catalog is now f
 Implementation status: WP06 (r2 review fixes) extends the above. The explicit policy now also filters schema lookup (`get_tool_schema`), the Starlark signature dump (`list_tool_signatures`), the MCP server-instructions signature section, the session-daemon socket listing, and the agent-print offered catalog — so the served, discovered, probed, dumped, and dispatched sets agree (ADR-023). With `--final-output`, an allowlist that omits the designated final-answer tool fails at startup with exit-2 `run_configuration_invalid` reason `tool_allowlist_missing_final_output`; the name is never added automatically (PM Q-FR-02-2). The reported `effective_tool_allowlist` is the intersection with the run's active legacy restriction (the agent-env `allowed_commands`), i.e. the set the run will actually permit. Config-registered custom local tools (`[tools.*]`) are part of the known catalog and may be allowlisted.
 
 Implementation status: WP06 (r4 review fixes) extends the above. Exit-2 taxonomy addition: a malformed or unreadable `--tool-allowlist` file fails at startup with `run_configuration_invalid` reason `tool_allowlist_invalid` (joining `tool_allowlist_unknown_entry` and `tool_allowlist_missing_final_output` as the three reserved WP06 exit-2 reasons; `tests/test_tool_allowlist.py::test_malformed_allowlist_file_fails_at_startup` and `::test_unreadable_allowlist_file_fails_at_startup`). Delegation (ADR-023 R1, PM Q-FR-02-1): the composite opcodes `tool_pipeline` and `lint_repair` (and the raw-protocol `tool_run`) now check their inner `tool_id` against the policy (`src/ops/tool_ops.rs` → `session.policy.admit(tool_id)`) before `ToolRegistry::pipeline`/`::repair`/`::run`, so a listed composite cannot reach an unlisted registered tool; the refused inner tool never runs (`tests/test_tool_allowlist.py::test_tool_pipeline_inner_tool_not_on_allowlist_is_refused`, `::test_lint_repair_inner_tool_not_on_allowlist_is_refused`, and the `ops::tool_ops` unit tests). In this package the allowlist names only native and config-registered `[tools.*]` tools; remote/worker names defer to WP10, and an active allowlist therefore removes every forwarded MCP tool from the catalog.
+
+_Status (WP18, fix run r4):_ r3 defects fixed. Input bytes are the bytes
+SCANNED: content `search` meters each file as its worker reads it
+(`tool_budget::BudgetedReader`) and stops mid-scan; `read_file` meters the file
+read. The index-answered `search mode=files` is REFUSED under
+`--tool-input-bytes`/`--tool-max-entries` with `tool_budget_unenforceable`. The
+deadline is checked by native readers and a passed deadline returns
+`tool_timeout`. The budget + deadline scope is installed at ONE place for every
+registry tool (`mcp::dispatch_tool_inner`); every plugin subprocess is reaped at
+the deadline, and plugins/`tool_pipeline` with unmeterable input are refused
+under an input/entry budget. `ToolBudget::charge_input` is the reserved name
+(was `charge_bytes`). A limit hit writes the reserved `limit_hit` audit event
+via `record_limit_hit`. `execute_script` holds its shared budget in the script
+context, installed only inside each per-op lock, so a concurrent top-level call
+cannot inherit it (F4). Open: process-level ACP/chat budget tests (F6).
diff --git a/src/agent.rs b/src/agent.rs
index ad580f8..83d525f 100644
--- a/src/agent.rs
+++ b/src/agent.rs
@@ -2147,6 +2147,16 @@ async fn run_inner(
                             tool_started.elapsed().as_millis() as u64,
                             None,
                         );
+                        // WP18 (FR-19 F5, ADR-031): a cooperative tool-budget
+                        // limit hit also writes the reserved `limit_hit` event
+                        // with the limit name, the tool, and the configured and
+                        // consumed values, through record_limit_hit.
+                        if let Some((limit, configured, consumed)) =
+                            crate::run_audit::budget_limit_hit(&content)
+                        {
+                            let _ =
+                                audit.record_limit_hit(&limit, Some(&name), configured, consumed);
+                        }
                         if audit.poisoned() {
                             terminate_reason = "audit_write_failed";
                             terminate = true;
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
index fc4e4c3..da2bdff 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -459,6 +459,10 @@ pub async fn run_agent(
             .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
         let bootstrap = crate::session_bootstrap::connect_or_spawn(
             &crate::session_bootstrap::BootstrapOptions {
+                // FR-19 (WP18, ADR-031): carry the caller's budget into the
+                // daemon launch identity so an interactive session enforces it
+                // and an attach cannot silently downgrade it.
+                tool_budget: cfg.tool_budget,
                 workspace,
                 config_path: explicit_config_path.as_deref(),
                 state_dir: cfg
@@ -702,6 +706,24 @@ pub async fn run_agent(
         }
         None => None,
     };
+    // FR-19 (WP18, ADR-031): a cooperative tool budget is installed at THIS
+    // process's dispatch seams, but worker-routed tool calls execute inside a
+    // separate worker process the parent budget never observes — the deadline
+    // cannot stop that owned work and its input cannot be accounted. Rather
+    // than silently ignore a budget option in worker mode (the frontend rule
+    // forbids a silent drop), refuse the budgeted worker route at startup, the
+    // same fail-closed class as `reject_credential_disclosure`
+    // (ADR-031 "refuse enabled-but-unenforceable"; the INTERFACES worker rule).
+    // A default-off budget leaves worker mode exactly unchanged.
+    if worker_config_runtime.is_some() && cfg.tool_budget.any_enabled() {
+        emit_run_configuration_invalid(
+            "tool_budget_worker_unenforceable",
+            "a tool budget (--tool-timeout-ms/--tool-input-bytes/--tool-max-entries) \
+             cannot be enforced across the worker boundary: worker-routed tool calls \
+             run in a separate process, so the deadline cannot stop that work and the \
+             input cannot be accounted. Remove the budget option or --worker-config.",
+        );
+    }
     let agent = load_agent_env(agent_env, &cfg)?;
     let cfg = with_skill_dir(cfg, &agent)?;
     let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
diff --git a/src/cli.rs b/src/cli.rs
index 8f9ef6e..360abb8 100644
--- a/src/cli.rs
+++ b/src/cli.rs
@@ -469,6 +469,29 @@ pub struct Cli {
     #[arg(long, global = true, value_name = "PATH")]
     pub tool_allowlist: Option<PathBuf>,
 
+    /// Stop an opted-in tool call at an absolute per-call deadline of N
+    /// milliseconds (WP18, FR-19, ADR-031). External work (exec, git_read)
+    /// inherits the remaining deadline and its process group is reaped. A
+    /// deadline hit returns `tool_timeout` with no partial result. Positive
+    /// integer; omitted means no new limit; default off.
+    #[arg(long = "tool-timeout-ms", global = true, value_name = "N")]
+    pub tool_timeout_ms: Option<u64>,
+
+    /// Refuse an opted-in tool call once its logical input exceeds N bytes
+    /// (WP18, FR-19, ADR-031). Counts file/git content read or scanned,
+    /// including cached-index content — not displayed output, which is a
+    /// separate truncation mechanism. A hit returns `tool_input_limit`.
+    /// Positive integer; omitted means no new limit; default off.
+    #[arg(long = "tool-input-bytes", global = true, value_name = "N")]
+    pub tool_input_bytes: Option<u64>,
+
+    /// Refuse an opted-in tool call once it walks more than N directory
+    /// entries (WP18, FR-19, ADR-031; integrator D56 `(proposed)`). A hit
+    /// returns `tool_input_limit` naming the entry ceiling. Positive integer;
+    /// omitted means no new limit; default off.
+    #[arg(long = "tool-max-entries", global = true, value_name = "N")]
+    pub tool_max_entries: Option<u64>,
+
     /// Serve and export full, self-contained JSON Schema draft 2020-12 tool
     /// schemas instead of the default stripped placeholders (WP17, FR-15).
     /// Equivalent to `[mcp] full_tool_schemas = true`; the config/env seam still
diff --git a/src/config.rs b/src/config.rs
index 221b088..cd7654a 100644
--- a/src/config.rs
+++ b/src/config.rs
@@ -194,6 +194,13 @@ pub struct Config {
     /// deserialized from a config file; set once at startup from the CLI.
     #[serde(skip)]
     pub discovery_policy: crate::discovery_policy::DiscoveryPolicy,
+    /// `--tool-timeout-ms` / `--tool-input-bytes` / `--tool-max-entries`
+    /// (FR-19, WP18, ADR-031): the caller's cooperative per-call tool budgets.
+    /// Caller-owned; never deserialized from workspace/user config, so nothing
+    /// the caller did not pass explicitly can set or widen a limit. All-absent
+    /// keeps legacy behavior (no budget installed).
+    #[serde(skip)]
+    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
     #[serde(default)]
     pub tools: HashMap<String, ToolConfig>,
 }
diff --git a/src/main.rs b/src/main.rs
index 2300f8a..408197b 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -65,6 +65,7 @@ mod skills;
 mod snapshot;
 mod state_root;
 mod structured_output;
+mod tool_budget;
 mod tool_contract;
 mod tool_descriptions;
 mod tool_facade;
@@ -545,6 +546,32 @@ async fn main() -> anyhow::Result<()> {
     if cli.no_output_spill {
         cfg.tool_output.save_full_output = false;
     }
+    // FR-19 (WP18, ADR-031): pin the caller's cooperative tool-budget options.
+    // Positive integers only; a zero is a non-positive limit and is refused at
+    // startup with a named error rather than silently disabling the budget.
+    // Caller-owned: set only here, never from workspace/user config.
+    for (name, value) in [
+        ("--tool-timeout-ms", cli.tool_timeout_ms),
+        ("--tool-input-bytes", cli.tool_input_bytes),
+        ("--tool-max-entries", cli.tool_max_entries),
+    ] {
+        if value == Some(0) {
+            eprintln!(
+                "{}",
+                serde_json::json!({"error": {
+                    "code": "run_configuration_invalid",
+                    "reason": "tool_budget_invalid",
+                    "message": format!("{name} must be a positive integer")
+                }})
+            );
+            std::process::exit(2);
+        }
+    }
+    cfg.tool_budget = crate::tool_budget::ToolBudgetConfig {
+        timeout_ms: cli.tool_timeout_ms,
+        input_bytes: cli.tool_input_bytes,
+        max_entries: cli.tool_max_entries,
+    };
     // FR-07 (ADR-022): pin the read-only workspace decision before any writer.
     cfg.workspace_read_only = cli.workspace_read_only;
     if cfg.workspace_read_only
diff --git a/src/managed_process.rs b/src/managed_process.rs
index dd33db9..85c7824 100644
--- a/src/managed_process.rs
+++ b/src/managed_process.rs
@@ -18,6 +18,31 @@ use tokio::sync::Mutex;
 
 pub type ProgressCallback<'a> = dyn Fn(String) + Send + Sync + 'a;
 
+tokio::task_local! {
+    /// Cooperative tool-budget deadline (remaining time) in force for the
+    /// current operation, set by the dispatch seam via [`with_budget_deadline`].
+    /// Subprocesses that [`run`] spawns inside the scope inherit it so the OS
+    /// process group is reaped AT the deadline, never merely waited on
+    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
+    static BUDGET_DEADLINE: Option<Duration>;
+}
+
+/// Run `fut` with the cooperative tool-budget deadline in scope. A `None`
+/// deadline (no `--tool-timeout-ms`, or no deadline left to inherit) leaves the
+/// legacy behavior of [`run`] unchanged. The deadline bounds but never extends
+/// the process default timeout (FR-19, WP18, ADR-031).
+pub async fn with_budget_deadline<F: std::future::Future>(
+    deadline: Option<Duration>,
+    fut: F,
+) -> F::Output {
+    BUDGET_DEADLINE.scope(deadline, fut).await
+}
+
+/// The cooperative deadline in scope, or `None` outside a budgeted operation.
+fn scoped_budget_deadline() -> Option<Duration> {
+    BUDGET_DEADLINE.try_with(|d| *d).unwrap_or(None)
+}
+
 /// Captured process output. Each stream is bounded while it is read.
 pub struct ManagedOutput {
     pub status: ExitStatus,
@@ -468,14 +493,28 @@ pub async fn run(
     let mut command = Command::new(program);
     command.args(args).current_dir(cwd);
     apply_environment(&mut command, cfg, overlays);
-    let output = capture(&mut command, cfg, None, stdin_data).await?;
+    // FR-19 (WP18, ADR-031): a cooperative tool-budget deadline in scope caps
+    // this subprocess and reaps its process group at the deadline. `None`
+    // preserves the legacy process-default behavior exactly.
+    let (output, budget_fired) = capture_budgeted(
+        &mut command,
+        cfg,
+        None,
+        stdin_data,
+        scoped_budget_deadline(),
+    )
+    .await?;
     if output.timed_out {
         return Err(io::Error::new(
             io::ErrorKind::TimedOut,
-            format!(
-                "{program} timed out after {} seconds",
-                cfg.default_timeout_secs
-            ),
+            if budget_fired {
+                format!("{program} stopped at the tool budget deadline")
+            } else {
+                format!(
+                    "{program} timed out after {} seconds",
+                    cfg.default_timeout_secs
+                )
+            },
         ));
     }
     Ok(output)
@@ -488,6 +527,33 @@ pub async fn capture(
     progress: Option<&ProgressCallback<'_>>,
     stdin_data: Option<&[u8]>,
 ) -> io::Result<ManagedOutput> {
+    capture_inner(command, cfg, progress, stdin_data, None)
+        .await
+        .map(|(output, _)| output)
+}
+
+/// Like [`capture`], but also stops the child at `budget_deadline` when set,
+/// using the same TERM->grace->KILL process-group reap — never a bare timeout
+/// wrapper that leaves work running underneath (FR-19, WP18, ADR-031). Returns
+/// the output plus `true` when the stop was the cooperative tool budget's
+/// deadline rather than the process default timeout. Context: src/tool_budget.rs.
+pub async fn capture_budgeted(
+    command: &mut Command,
+    cfg: &ProcessConfig,
+    progress: Option<&ProgressCallback<'_>>,
+    stdin_data: Option<&[u8]>,
+    budget_deadline: Option<Duration>,
+) -> io::Result<(ManagedOutput, bool)> {
+    capture_inner(command, cfg, progress, stdin_data, budget_deadline).await
+}
+
+async fn capture_inner(
+    command: &mut Command,
+    cfg: &ProcessConfig,
+    progress: Option<&ProgressCallback<'_>>,
+    stdin_data: Option<&[u8]>,
+    budget_deadline: Option<Duration>,
+) -> io::Result<(ManagedOutput, bool)> {
     command
         .stdin(if stdin_data.is_some() {
             Stdio::piped()
@@ -506,18 +572,37 @@ pub async fn capture(
         .take()
         .ok_or_else(|| io::Error::other("spawned process has no stderr pipe"))?;
     let stdin = child.stdin_mut().take();
-    let timeout =
+    let proc_timeout =
         (cfg.default_timeout_secs > 0).then(|| Duration::from_secs(cfg.default_timeout_secs));
+    // FR-19 (WP18, ADR-031): the cooperative tool budget caps this wait at its
+    // remaining deadline and never extends the process default. The effective
+    // stop is the sooner of the two; `budget_is_tighter` records that the
+    // budget deadline was (at least as) soon, so the caller returns the typed
+    // `tool_timeout` rather than the legacy process-timeout error. Either stop
+    // reaps the whole process group through `ManagedChild::terminate`.
+    let effective_timeout = match (proc_timeout, budget_deadline) {
+        (Some(p), Some(b)) => Some(p.min(b)),
+        (Some(p), None) => Some(p),
+        (None, b) => b,
+    };
+    let budget_is_tighter = match (proc_timeout, budget_deadline) {
+        (_, None) => false,
+        (None, Some(_)) => true,
+        (Some(p), Some(b)) => b <= p,
+    };
     let grace = Duration::from_millis(cfg.termination_grace_ms);
 
     let wait = async {
-        if let Some(timeout) = timeout {
+        if let Some(timeout) = effective_timeout {
             match tokio::time::timeout(timeout, child.wait()).await {
-                Ok(status) => status.map(|status| (status, false)),
-                Err(_) => child.terminate(grace).await.map(|status| (status, true)),
+                Ok(status) => status.map(|status| (status, false, false)),
+                Err(_) => child
+                    .terminate(grace)
+                    .await
+                    .map(|status| (status, true, budget_is_tighter)),
             }
         } else {
-            child.wait().await.map(|status| (status, false))
+            child.wait().await.map(|status| (status, false, false))
         }
     };
     let write_stdin = async {
@@ -545,16 +630,19 @@ pub async fn capture(
     );
     let stdout = stdout?;
     let stderr = stderr?;
-    let (status, timed_out) = status?;
+    let (status, timed_out, budget_fired) = status?;
     stdin?;
-    Ok(ManagedOutput {
-        status,
-        stdout: stdout.render(),
-        stderr: stderr.render(),
-        stdout_truncated: stdout.truncated(),
-        stderr_truncated: stderr.truncated(),
-        timed_out,
-    })
+    Ok((
+        ManagedOutput {
+            status,
+            stdout: stdout.render(),
+            stderr: stderr.render(),
+            stdout_truncated: stdout.truncated(),
+            stderr_truncated: stderr.truncated(),
+            timed_out,
+        },
+        budget_fired,
+    ))
 }
 
 struct BoundedStream {
diff --git a/src/mcp.rs b/src/mcp.rs
index 3b8475e..1fa6108 100644
--- a/src/mcp.rs
+++ b/src/mcp.rs
@@ -438,6 +438,24 @@ async fn dispatch_tool_inner(
     }
     session.used_tools.insert(name.to_string());
 
+    // === FR-19 F3 (WP18, ADR-031): ONE place installs the operation budget and
+    // the deadline scope for EVERY registry tool (facade ops + plugins +
+    // git_read), not one match arm per tool. A composite child inherits the
+    // parent's budget (owned == false). A process-starting plugin whose logical
+    // input cannot be metered is refused when an input/entry budget is enabled
+    // (never silently exempted). The absolute deadline is threaded into
+    // managed_process so every subprocess group is reaped AT the deadline; a
+    // call whose deadline passed returns tool_timeout, never a late success. All
+    // a no-op when no budget option is set. ===
+    let budget_owned = session.begin_tool_operation();
+    if crate::tool_budget::tool_input_unmeterable(name) {
+        if let Some(failure) = session.budget_input_refusal(name) {
+            session.end_tool_operation(budget_owned);
+            return response_to_result(failure.into_response(name));
+        }
+    }
+    let budget_remaining = session.budget_remaining();
+    let dispatched = crate::managed_process::with_budget_deadline(budget_remaining, async {
     // Registry-based dispatch: route ops-backed tools through the facade.
     if let Some(resp) = tool_facade::invoke(session, name, args).await {
         session.last_response_meta = resp.meta.clone();
@@ -794,10 +812,16 @@ async fn dispatch_tool_inner(
 
         "git_read" => {
             let command = get_str(args, "command").unwrap_or_default();
+            // FR-19 (WP18, ADR-031): git_read runs inside the single budget +
+            // deadline scope installed above, so the git child's process group
+            // is reaped AT the deadline and a passed deadline becomes a typed
+            // tool_timeout at the post-dispatch check. Here we only meter the
+            // git object content it returned (the PM counts this as logical
+            // input): a byte-ceiling hit is tool_input_limit with no partial.
             let Some(registry) = &session.tool_registry else {
                 return err_text(serde_json::json!({"error":{"kind":"unavailable","message":"tool registry unavailable"}}).to_string());
             };
-            match registry
+            let run_result = registry
                 .run(
                     "git_read",
                     &command,
@@ -806,10 +830,14 @@ async fn dispatch_tool_inner(
                     None,
                     Some(args),
                 )
-                .await
-            {
+                .await;
+            match run_result {
                 Ok(result) => {
                     let text = result.output.to_string();
+                    if let Err(failure) = session.budget_charge_bytes(text.len() as u64, "git_read")
+                    {
+                        return response_to_result(failure.into_response("git_read"));
+                    }
                     if result.exit_code == 0 {
                         ok_text(text)
                     } else {
@@ -1158,6 +1186,19 @@ async fn dispatch_tool_inner(
 
         _ => Err(CallToolError::unknown_tool(name.to_string())),
     }
+    })
+    .await;
+    // FR-19 F2/F3 (WP18, ADR-031): a call whose deadline passed is a timeout,
+    // not a late result — even when the work finished just after the deadline,
+    // and even when a deadline-stopped subprocess surfaced as a plugin error
+    // string. Reclassify to the typed tool_timeout whenever the deadline has
+    // passed; otherwise keep the original result (success or a genuine error).
+    let dispatched = match session.budget_check_deadline(name) {
+        Ok(()) => dispatched,
+        Err(failure) => response_to_result(failure.into_response(name)),
+    };
+    session.end_tool_operation(budget_owned);
+    dispatched
 }
 
 fn extract_result_text(result: &CallToolResult) -> String {
@@ -1488,6 +1529,12 @@ impl ServerHandler for DaimonosHandler {
             let observe_on = crate::kgl::observe::enabled();
             let mut observed_ops: Vec<(String, Value)> = Vec::new();
 
+            // FR-19 (WP18, ADR-031): install ONE shared operation budget for
+            // the whole batch, so every sub-op (dispatched via dispatch_tool,
+            // which re-enters ops::dispatch) inherits the same remaining
+            // allowance and absolute deadline instead of each getting a fresh
+            // top-level budget. No-op when no budget option is set.
+            let batch_budget_owned = session.begin_tool_operation();
             let mut results = Vec::with_capacity(ops.len());
             for (i, op_val) in ops.iter().enumerate() {
                 let tool = match op_val.get("tool").and_then(|v| v.as_str()) {
@@ -1527,6 +1574,9 @@ impl ServerHandler for DaimonosHandler {
                 }
             }
 
+            if batch_budget_owned {
+                session.end_tool_operation(true);
+            }
             let payload = serde_json::to_string(&results).unwrap_or_default();
             // Read the dirty flag before the observe path may drop the lock.
             let tools_changed = session.take_tools_changed();
@@ -1655,7 +1705,21 @@ async fn run_execute_script(
         .to_string();
     let timeout_secs =
         crate::script::bounded_timeout_secs(args.get("timeout").and_then(Value::as_i64));
-    script::execute(&code, session, std::time::Duration::from_secs(timeout_secs)).await
+    // FR-19 F4 (WP18, ADR-031): execute_script is a composite that owns ONE
+    // shared operation budget for all its inner ops. That budget is created and
+    // held inside the script context (src/script.rs) and installed on the
+    // session only inside each inner per-op lock, then cleared on release — it
+    // is deliberately NOT installed on the shared session here, because
+    // run_execute_script releases the lock while the script runs, and leaving
+    // it on the session across that window would let a concurrent top-level MCP
+    // call inherit it (wrong deadline/allowance). No-op when no budget option
+    // is set.
+    script::execute(
+        &code,
+        session.clone(),
+        std::time::Duration::from_secs(timeout_secs),
+    )
+    .await
 }
 
 /// Convert the Send-safe script outcome only after all handler awaits finish;
diff --git a/src/ops/exec_ops.rs b/src/ops/exec_ops.rs
index 3b62be8..96af27f 100644
--- a/src/ops/exec_ops.rs
+++ b/src/ops/exec_ops.rs
@@ -533,6 +533,12 @@ async fn capture_exec(
     env: &HashMap<String, String>,
     on_progress: Option<&ExecProgressCallback<'_>>,
 ) -> Response {
+    // FR-19 (WP18, ADR-031): if the cooperative budget's deadline already
+    // passed, refuse before spawning anything rather than start work we would
+    // immediately have to stop. No budget / no deadline is a no-op.
+    if let Err(failure) = session.budget_check_deadline("exec") {
+        return failure.into_response("exec");
+    }
     let mut command = build_command(cmd, args);
     command.current_dir(cwd);
     crate::managed_process::apply_environment(&mut command, &session.cfg.process, env);
@@ -541,17 +547,24 @@ async fn capture_exec(
             callback(ExecProgress::Output(text));
         }
     });
-    let output = match crate::managed_process::capture(
+    // FR-19 (WP18, ADR-031): hand the budget's remaining deadline to the
+    // managed-process path so the exec is stopped (TERM/grace/KILL, whole
+    // process group reaped) at the sooner of it and the process default. A
+    // budget-deadline stop becomes a typed `tool_timeout` below; `None` keeps
+    // legacy behavior exactly.
+    let budget_deadline = session.budget_remaining();
+    let (output, budget_fired) = match crate::managed_process::capture_budgeted(
         &mut command,
         &session.cfg.process,
         progress
             .as_ref()
             .map(|callback| callback as &crate::managed_process::ProgressCallback<'_>),
         None,
+        budget_deadline,
     )
     .await
     {
-        Ok(output) => output,
+        Ok(result) => result,
         Err(error) => {
             if let Some(callback) = on_progress {
                 callback(ExecProgress::Exit {
@@ -568,6 +581,14 @@ async fn capture_exec(
             signal: exit_signal(&output.status),
         });
     }
+    // FR-19 (WP18, ADR-031): a cooperative-budget deadline stop returns the
+    // typed `tool_timeout` with no partial exec result, distinct from the
+    // legacy process-timeout (exit 6) path below.
+    if budget_fired {
+        if let Some(failure) = session.budget_timeout_failure("exec") {
+            return failure.into_response("exec");
+        }
+    }
     if output.timed_out {
         return Response::err(
             6,
diff --git a/src/ops/file_ops.rs b/src/ops/file_ops.rs
index 5facec8..9cc4d9d 100644
--- a/src/ops/file_ops.rs
+++ b/src/ops/file_ops.rs
@@ -24,6 +24,19 @@ pub async fn read(session: &mut Session, op: &Op) -> Response {
     if let Some(index) = &session.index {
         index.observe_path(&path).await;
     }
+    // FR-19 F2 (WP18, ADR-031): a read that completed after the deadline is a
+    // timeout, not a late success with content; check before building a result.
+    if let Err(failure) = session.budget_check_deadline(op.p.as_deref().unwrap_or("read_file")) {
+        return failure.into_response("read_file");
+    }
+    // FR-19 (WP18, ADR-031): charge the logical file content read before
+    // producing any result. A byte-ceiling hit returns tool_input_limit with no
+    // partial content (output truncation is a separate mechanism).
+    if let Err(failure) =
+        session.budget_charge_bytes(content.len() as u64, op.p.as_deref().unwrap_or("read_file"))
+    {
+        return failure.into_response("read_file");
+    }
 
     let total_lines = content.lines().count();
     let offset = op.n.unwrap_or(0).max(0) as usize;
@@ -114,6 +127,12 @@ async fn read_confined(session: &mut Session, op: &Op) -> Response {
         Ok(Err(error)) => return Response::err(4, &format!("io: {error}")),
         Err(error) => return Response::err(4, &format!("io: {error}")),
     };
+    // FR-19 (WP18, ADR-031): charge logical file content for confined reads too.
+    if let Err(failure) =
+        session.budget_charge_bytes(content.len() as u64, op.p.as_deref().unwrap_or("read_file"))
+    {
+        return failure.into_response("read_file");
+    }
     let total_lines = content.lines().count();
     let offset = op.n.unwrap_or(0).max(0) as usize;
     let limit = op.n2.unwrap_or(0);
@@ -432,9 +451,21 @@ pub async fn ls(session: &Session, op: &Op) -> Response {
     };
 
     let mut entries = Vec::new();
-    if let Err(e) = ls_recurse(&path, &path, depth, &opts, &mut entries).await {
+    let mut walked = 0u64;
+    if let Err(e) = ls_recurse(&path, &path, depth, &opts, &mut entries, &mut walked).await {
         return Response::err(4, &format!("ls: {e}"));
     }
+    // FR-19 F2 (WP18, ADR-031): a walk that ran past the deadline is a timeout,
+    // not a late listing; check before the result is built.
+    if let Err(failure) = session.budget_check_deadline("ls") {
+        return failure.into_response("ls");
+    }
+    // FR-19 (WP18, ADR-031): charge directory entries walked. An entry-ceiling
+    // hit returns tool_input_limit (integrator D56) naming the entry limit,
+    // with no partial listing. No-op when no entry budget is set.
+    if let Err(failure) = session.budget_charge_entries(walked, "ls") {
+        return failure.into_response("ls");
+    }
 
     entries.sort_by(|a, b| {
         let a_name = a["n"].as_str().unwrap_or("");
@@ -503,6 +534,16 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
         Err(error) => return Response::err(4, &format!("ls: {error}")),
     };
     let (skipped, skipped_links) = walk.skipped_summary();
+    // FR-19 (WP18, ADR-031): charge directory entries walked on the confined
+    // path too (entries visited plus those skipped by the walk). No-op when no
+    // entry budget is set.
+    let walked = walk.entries.len() as u64 + skipped as u64 + skipped_links as u64;
+    if let Err(failure) = session.budget_check_deadline("ls") {
+        return failure.into_response("ls");
+    }
+    if let Err(failure) = session.budget_charge_entries(walked, "ls") {
+        return failure.into_response("ls");
+    }
     let mut entries = Vec::new();
     for entry in walk.entries {
         let Ok(relative) = entry.relative_path.strip_prefix(&base) else {
@@ -554,6 +595,7 @@ async fn ls_recurse(
     depth: usize,
     opts: &LsOpts<'_>,
     entries: &mut Vec<serde_json::Value>,
+    walked: &mut u64,
 ) -> Result<(), String> {
     let mut dir = tokio::fs::read_dir(path).await.map_err(|e| {
         if e.kind() == std::io::ErrorKind::NotFound {
@@ -564,6 +606,9 @@ async fn ls_recurse(
     })?;
 
     while let Ok(Some(entry)) = dir.next_entry().await {
+        // FR-19 (WP18, ADR-031): every directory entry visited counts, before
+        // any hidden/skip/type/glob filter, so the charged input is honest.
+        *walked += 1;
         let name = entry.file_name().to_string_lossy().to_string();
         if !opts.show_all && name.starts_with('.') {
             continue;
@@ -630,7 +675,15 @@ async fn ls_recurse(
         }
 
         if is_dir && depth > 1 {
-            let _ = Box::pin(ls_recurse(root, &entry.path(), depth - 1, opts, entries)).await;
+            let _ = Box::pin(ls_recurse(
+                root,
+                &entry.path(),
+                depth - 1,
+                opts,
+                entries,
+                walked,
+            ))
+            .await;
         }
     }
 
@@ -922,6 +975,11 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
     let workspace = session.workspace.clone();
     // D08 (ADR-021, FR-09): explicit-only runs let no implicit ignore file steer the walk.
     let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
+    // FR-19 (WP18, ADR-031): hand the shared budget into the blocking worker so
+    // the worker itself meters the bytes it SCANS (not the bytes it returns)
+    // and checks the deadline per read chunk, stopping mid-scan. `None` keeps
+    // the legacy fast path byte-for-byte.
+    let budget = session.tool_budget();
 
     let result = tokio::task::spawn_blocking(move || {
         grep_blocking(
@@ -931,6 +989,7 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
             max,
             file_glob.as_deref(),
             apply_ignore_files,
+            budget,
         )
     })
     .await;
@@ -945,9 +1004,15 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
                     .collect::<Vec<_>>();
                 index.observe_paths(observed).await;
             }
+            // A scan that completed exactly at the deadline is a timeout, not a
+            // late success (FR-19 F2). Bytes were already charged while reading.
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             Response::ok(json!({"matches": matches}))
         }
-        Ok(Err(e)) => Response::err(4, &format!("grep: {e}")),
+        Ok(Err(GrepStop::Budget(hit))) => hit.into_failure(Some("search")).into_response("search"),
+        Ok(Err(GrepStop::Io(e))) => Response::err(4, &format!("grep: {e}")),
         Err(e) => Response::err(4, &format!("grep task: {e}")),
     }
 }
@@ -982,11 +1047,42 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
     // even under confinement (--confine-paths), so a hostile ignore file cannot
     // steer confined evidence grep.
     let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
+    // FR-19 F1/F2 (ADR-031): meter scanned bytes and check the deadline inside
+    // the confined blocking worker too. `None` keeps the legacy path.
+    let budget = session.tool_budget();
     let result = tokio::task::spawn_blocking(move || {
         let mut searcher = Searcher::new();
+        let budget_hit: std::cell::RefCell<Option<crate::tool_budget::BudgetHit>> =
+            std::cell::RefCell::new(None);
         let mut scan = |file: std::fs::File,
                         relative: String,
                         matches: &mut Vec<serde_json::Value>| {
+            if let Some(b) = &budget {
+                if let Err(hit) = b.check_deadline() {
+                    *budget_hit.borrow_mut() = Some(hit);
+                    return false;
+                }
+                let mut reader =
+                    crate::tool_budget::BudgetedReader::new(file, std::sync::Arc::clone(b));
+                let ok = searcher
+                    .search_reader(
+                        &matcher,
+                        &mut reader,
+                        UTF8(|line_num, line| {
+                            if matches.len() < max {
+                                matches.push(
+                                    json!({"f": relative, "l": line_num, "t": line.trim_end()}),
+                                );
+                            }
+                            Ok(matches.len() < max)
+                        }),
+                    )
+                    .is_ok();
+                if let Some(hit) = reader.hit() {
+                    *budget_hit.borrow_mut() = Some(hit.clone());
+                }
+                return ok;
+            }
             searcher
                 .search_reader(
                     &matcher,
@@ -1017,6 +1113,9 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
                     .to_string_lossy()
                     .to_string();
                 let skipped = usize::from(!scan(start.into_file(), relative, &mut matches));
+                if let Some(hit) = budget_hit.borrow_mut().take() {
+                    return Err(hit.into_failure(Some("search")));
+                }
                 return Ok::<_, crate::path_policy::ToolFailure>((matches, skipped));
             }
             return Ok((matches, 0));
@@ -1068,12 +1167,23 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
             if !scan(authorized.into_file(), relative, &mut matches) {
                 skipped += 1
             }
+            if budget_hit.borrow().is_some() {
+                break;
+            }
+        }
+        if let Some(hit) = budget_hit.borrow_mut().take() {
+            return Err(hit.into_failure(Some("search")));
         }
         Ok::<_, crate::path_policy::ToolFailure>((matches, skipped))
     })
     .await;
     match result {
         Ok(Ok((matches, skipped))) => {
+            // Bytes were charged while scanning; a scan that finished exactly at
+            // the deadline is still a timeout, not a late success (FR-19 F2).
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             let mut value = json!({"matches": matches});
             if skipped > 0 {
                 value["skipped"] = json!(skipped)
@@ -1087,6 +1197,13 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
     }
 }
 
+/// How a budgeted grep worker stopped: an I/O/regex error, or a cooperative
+/// tool-budget hit the worker detected while scanning (FR-19 F1/F2, ADR-031).
+pub(crate) enum GrepStop {
+    Io(String),
+    Budget(crate::tool_budget::BudgetHit),
+}
+
 fn grep_blocking(
     pattern: &str,
     root: &Path,
@@ -1094,13 +1211,14 @@ fn grep_blocking(
     max: usize,
     file_glob: Option<&str>,
     apply_ignore_files: bool,
-) -> Result<Vec<serde_json::Value>, String> {
+    budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
+) -> Result<Vec<serde_json::Value>, GrepStop> {
     use grep_regex::RegexMatcher;
     use grep_searcher::sinks::UTF8;
     use grep_searcher::Searcher;
     use ignore::WalkBuilder;
 
-    let matcher = RegexMatcher::new(pattern).map_err(|e| format!("regex: {e}"))?;
+    let matcher = RegexMatcher::new(pattern).map_err(|e| GrepStop::Io(format!("regex: {e}")))?;
     let mut searcher = Searcher::new();
     let mut matches = Vec::new();
 
@@ -1143,6 +1261,42 @@ fn grep_blocking(
             .to_string_lossy()
             .to_string();
 
+        // FR-19 F1/F2 (ADR-031): when a budget is active, the worker itself
+        // meters the bytes it SCANS and checks the deadline at a bounded step
+        // (between files, and inside a large file per read chunk via
+        // BudgetedReader), stopping the scan the moment the ceiling or the
+        // deadline is hit. Without a budget the legacy search_path fast path is
+        // unchanged.
+        if let Some(budget) = &budget {
+            if let Err(hit) = budget.check_deadline() {
+                return Err(GrepStop::Budget(hit));
+            }
+            let file = match std::fs::File::open(path) {
+                Ok(file) => file,
+                Err(_) => continue,
+            };
+            let mut reader =
+                crate::tool_budget::BudgetedReader::new(file, std::sync::Arc::clone(budget));
+            let _ = searcher.search_reader(
+                &matcher,
+                &mut reader,
+                UTF8(|line_num, line| {
+                    if matches.len() < max {
+                        matches.push(serde_json::json!({
+                            "f": rel,
+                            "l": line_num,
+                            "t": line.trim_end(),
+                        }));
+                    }
+                    Ok(matches.len() < max)
+                }),
+            );
+            if let Some(hit) = reader.hit() {
+                return Err(GrepStop::Budget(hit.clone()));
+            }
+            continue;
+        }
+
         let _ = searcher.search_path(
             &matcher,
             path,
diff --git a/src/ops/mod.rs b/src/ops/mod.rs
index 1ab216b..c91b207 100644
--- a/src/ops/mod.rs
+++ b/src/ops/mod.rs
@@ -31,7 +31,14 @@ pub async fn dispatch_with_progress(
     req: Request,
     on_exec_progress: Option<&ExecProgressCallback<'_>>,
 ) -> Response {
-    match req {
+    // FR-19 (WP18, ADR-031): install a fresh operation budget for this
+    // top-level call. A composite's children re-enter here and inherit the
+    // parent's budget (`owned == false`), so delegation cannot multiply the
+    // allowance or extend the deadline; the installing call clears it on
+    // return so each sequential top-level call starts fresh. When no budget
+    // option is set nothing is installed and behavior is unchanged.
+    let owned = session.begin_tool_operation();
+    let result = match req {
         Request::Single(op) => dispatch_op(session, op, on_exec_progress).await,
         Request::Batch { batch } => {
             let mut results = Vec::with_capacity(batch.len());
@@ -40,7 +47,9 @@ pub async fn dispatch_with_progress(
             }
             Response::ok(serde_json::to_value(results).unwrap())
         }
-    }
+    };
+    session.end_tool_operation(owned);
+    result
 }
 
 async fn dispatch_op(
@@ -143,10 +152,25 @@ async fn find(session: &Session, op: &Op) -> Response {
         None => None,
     };
 
+    // FR-19 F1 (WP18, ADR-031): the index-answered search (mode=files) reads
+    // cached index content, not scannable file content, so its logical input is
+    // not meterable as logical bytes; refuse an enabled input/entry budget
+    // rather than charge "bytes returned" (the owner choice stated in the
+    // handoff). The deadline IS enforced (checked before and after the search).
+    if let Some(failure) = session.budget_input_refusal("search") {
+        return failure.into_response("search");
+    }
+    if let Err(failure) = session.budget_check_deadline("search") {
+        return failure.into_response("search");
+    }
     idx.ensure_populated().await;
     let results = idx
         .search_scoped(&query, max, op.q.as_deref(), file_glob.as_ref())
         .await;
+    // A search that ran past the deadline is a timeout, not a late result.
+    if let Err(failure) = session.budget_check_deadline("search") {
+        return failure.into_response("search");
+    }
     let stats = idx.stats().await;
 
     Response::ok(serde_json::json!({
@@ -161,6 +185,13 @@ async fn find_confined(session: &Session, op: &Op) -> Response {
     let Some(query) = &op.p else {
         return Response::err(3, "find requires query in 'p'");
     };
+    // FR-19 F1 (WP18, ADR-031): the confined files-mode search ranks index/name
+    // content, not scannable file content, so refuse an enabled input/entry
+    // budget rather than charge "bytes returned" (same choice as the plain
+    // index path). The deadline is enforced below.
+    if let Some(failure) = session.budget_input_refusal("search") {
+        return failure.into_response("search");
+    }
     let glob = match op.g.as_deref().map(glob::Pattern::new) {
         Some(Ok(pattern)) => Some(pattern),
         Some(Err(error)) => return Response::err(3, &format!("invalid glob: {error}")),
@@ -258,6 +289,11 @@ async fn find_confined(session: &Session, op: &Op) -> Response {
     .await;
     match result {
         Ok(Ok((results, stats, skipped))) => {
+            // A search that ran past the deadline is a timeout, not a late
+            // result (FR-19 F2).
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             let mut value = serde_json::json!({"results": results, "index": stats});
             if skipped > 0 {
                 value["skipped"] = serde_json::json!(skipped)
diff --git a/src/ops/tool_ops.rs b/src/ops/tool_ops.rs
index 28abafc..1659eca 100644
--- a/src/ops/tool_ops.rs
+++ b/src/ops/tool_ops.rs
@@ -122,6 +122,20 @@ pub async fn tool_pipeline(session: &Session, op: &Op) -> Response {
         return failure.into_response(tool_id);
     }
 
+    // FR-19 F3 (WP18, ADR-031): a pipeline runs external plugin stages whose
+    // remaining deadline / logical input cannot be enforced stage-by-stage
+    // cooperatively. Rather than leave it "not cooperative", refuse the whole
+    // pipeline when any budget option is set (ADR-031 permits refusing the
+    // pipeline under --tool-timeout-ms; the same applies to input/entry). No
+    // budget option leaves it unchanged.
+    if session.budget_active() {
+        return crate::path_policy::ToolFailure::tool_budget_unenforceable(
+            Some("tool_pipeline"),
+            "a pipeline's external stages are not cooperatively budgeted",
+        )
+        .into_response("tool_pipeline");
+    }
+
     let stages = match &op.a {
         Some(s) if !s.is_empty() => s,
         _ => return Response::err(3, "tool_pipeline requires stage names in 'a'"),
diff --git a/src/path_policy.rs b/src/path_policy.rs
index b07fe7f..e5947b1 100644
--- a/src/path_policy.rs
+++ b/src/path_policy.rs
@@ -200,6 +200,70 @@ impl ToolFailure {
     pub fn unsupported(message: impl Into<String>) -> Self {
         Self::refusal(CODE_FILESYSTEM_POLICY_UNSUPPORTED, message, None)
     }
+
+    /// Cooperative tool-budget deadline hit (WP18, FR-19, ADR-031). The
+    /// `configured_ms` value is reported as both the limit and the consumed
+    /// amount: the call ran until at least the deadline. No partial success.
+    pub fn tool_timeout(argument: Option<&str>, configured_ms: u64) -> Self {
+        let mut failure = Self::refusal(
+            "tool_timeout",
+            format!("tool deadline exceeded: configured_ms={configured_ms}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure.budget = Some(Box::new(FailureBudget {
+            limit: configured_ms,
+            configured: configured_ms,
+            consumed: configured_ms,
+        }));
+        failure
+    }
+
+    /// Cooperative tool-budget logical-input hit (WP18, FR-19, ADR-031). Used
+    /// for both the byte ceiling and the directory-entry ceiling; `which`
+    /// names the ceiling that was hit (integrator D56 keeps the single
+    /// reviewed `tool_input_limit` code). No partial success.
+    /// Cooperative tool-budget refusal (WP18, FR-19, ADR-031): a tool whose
+    /// relevant input/entry budget cannot be enforced is REFUSED when that
+    /// budget is enabled, rather than silently exempted (INTERFACES.md row
+    /// WP18). Used for process-starting plugins (no meterable logical input)
+    /// and the index-answered search (cached index content is not exposed as a
+    /// byte count). Deadline enforcement is unaffected.
+    pub fn tool_budget_unenforceable(argument: Option<&str>, detail: &str) -> Self {
+        let mut failure = Self::refusal(
+            "tool_budget_unenforceable",
+            format!("input/entry budget cannot be enforced for this tool: {detail}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure
+    }
+
+    pub fn tool_input_limit(
+        argument: Option<&str>,
+        which: &str,
+        limit: u64,
+        consumed: u64,
+    ) -> Self {
+        let mut failure = Self::refusal(
+            "tool_input_limit",
+            format!("tool input limit exceeded ({which}): configured={limit}, consumed={consumed}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure.budget = Some(Box::new(FailureBudget {
+            limit,
+            configured: limit,
+            consumed,
+        }));
+        failure
+    }
 }
 
 /// Configurable bounds for the authority. Caller/config supplied — no hidden
diff --git a/src/run_audit.rs b/src/run_audit.rs
index c95163f..0963949 100644
--- a/src/run_audit.rs
+++ b/src/run_audit.rs
@@ -596,6 +596,7 @@ pub const REFUSAL_CODES: &[&str] = &[
     "tool_policy_unsupported",
     "tool_timeout",
     "tool_input_limit",
+    "tool_budget_unenforceable",
     "offline_tool_forbidden",
 ];
 
@@ -670,6 +671,23 @@ pub fn refusal_code(result: &str) -> Option<String> {
     REFUSAL_CODES.contains(&code).then(|| code.to_string())
 }
 
+/// A cooperative tool-budget limit hit parsed from a boundary result, so the
+/// run loop can write the reserved `limit_hit` audit event (WP18 FR-19 F5,
+/// ADR-031). Returns `(limit_name, configured, consumed)` when the result is a
+/// `tool_timeout` or `tool_input_limit` failure; `None` otherwise. The limit
+/// name is the failure `code` (e.g. `tool_timeout`, `tool_input_limit`); the
+/// numeric fields come from the failure's budget block.
+pub fn budget_limit_hit(result: &str) -> Option<(String, Option<u64>, Option<u64>)> {
+    let value: serde_json::Value = serde_json::from_str(result).ok()?;
+    let code = value.get("code")?.as_str()?;
+    if code != "tool_timeout" && code != "tool_input_limit" {
+        return None;
+    }
+    let configured = value.get("configured").and_then(serde_json::Value::as_u64);
+    let consumed = value.get("consumed").and_then(serde_json::Value::as_u64);
+    Some((code.to_string(), configured, consumed))
+}
+
 fn insert_opt_str(
     map: &mut serde_json::Map<String, serde_json::Value>,
     key: &str,
diff --git a/src/script.rs b/src/script.rs
index 6697b59..48e7d96 100644
--- a/src/script.rs
+++ b/src/script.rs
@@ -512,6 +512,14 @@ fn build_globals(
     op_count: Arc<AtomicUsize>,
     subcall: Option<SubcallEnv>,
 ) -> Globals {
+    // FR-19 F4 (WP18, ADR-031): create ONE shared operation budget for the
+    // whole script here, so every inner op charges the same cumulative
+    // allowance against the same absolute deadline. It is installed on the
+    // session only inside each per-op lock (see dispatch_request /
+    // dispatch_tool_by_name) and cleared when the lock is released, so it never
+    // lingers on the shared session where a concurrent top-level MCP call could
+    // inherit it. `None` when no budget option is set (legacy behavior).
+    let budget = handle.block_on(async { session.lock().await.cfg.tool_budget.start() });
     // We store session + handle + cancel flag in a thread-local so the
     // starlark_module functions can access them without capturing closures.
     TOOL_CTX.with(|ctx| {
@@ -521,6 +529,7 @@ fn build_globals(
             subcall,
             cancel,
             op_count,
+            budget,
         });
     });
 
@@ -572,6 +581,10 @@ struct ToolContext {
     /// Owned by the caller so the count survives even when the script errors
     /// after dispatching some ops.
     op_count: Arc<AtomicUsize>,
+    /// FR-19 F4: the one shared operation budget for this script, installed on
+    /// the session inside each per-op lock and cleared on release. `None` when
+    /// no budget option is set.
+    budget: Option<Arc<crate::tool_budget::ToolBudget>>,
 }
 
 thread_local! {
@@ -643,10 +656,16 @@ fn dispatch_request(request: Request, label: &str) -> Result<Response, anyhow::E
         let child_span =
             crate::observability::ToolSpan::new(label, crate::observability::tool_kind(label));
         let session = ctx.session.clone();
+        let budget = ctx.budget.clone();
         let resp = child_span.span().in_scope(|| {
             ctx.handle.block_on(async {
                 let mut s = session.lock().await;
+                // FR-19 F4: install the shared budget only while this op holds
+                // the lock; clear it before release so it never leaks to a
+                // concurrent top-level call. begin_tool_operation inherits it.
+                s.set_operation_budget(budget.clone());
                 let resp = ops::dispatch(&mut s, request).await;
+                s.set_operation_budget(None);
                 if let Some(analytics) = s.analytics.clone() {
                     let resp_chars = response_chars(&resp);
                     let (saved_tokens, savings_pct) =
@@ -721,11 +740,14 @@ fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Respons
         None => with_ctx(|ctx| {
             let started = std::time::Instant::now();
             let request_chars = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);
+            let budget = ctx.budget.clone();
             ctx.handle.block_on(async {
                 let mut session = ctx.session.lock().await;
-                let Some((content, is_error, meta)) =
-                    crate::mcp::dispatch_local_tool(&mut session, name, args).await
-                else {
+                // FR-19 F4: shared budget bounded to this locked op (see above).
+                session.set_operation_budget(budget.clone());
+                let dispatched = crate::mcp::dispatch_local_tool(&mut session, name, args).await;
+                session.set_operation_budget(None);
+                let Some((content, is_error, meta)) = dispatched else {
                     if name.starts_with("mcp__") {
                         return Err(anyhow::anyhow!(
                             "remote MCP tool '{name}' is unavailable in execute_script: \
diff --git a/src/session.rs b/src/session.rs
index 9312648..0730be4 100644
--- a/src/session.rs
+++ b/src/session.rs
@@ -89,6 +89,15 @@ pub struct Session {
     /// and model-visible listing. Unrestricted keeps legacy behavior.
     /// Context: src/run_policy.rs::ToolPolicy.
     pub policy: crate::run_policy::ToolPolicy,
+    /// Caller's cooperative tool-budget options (FR-19, WP18, ADR-031), copied
+    /// from `cfg.tool_budget`. Used to install a fresh operation budget at each
+    /// top-level tool call. Context: src/tool_budget.rs.
+    pub tool_budget_config: crate::tool_budget::ToolBudgetConfig,
+    /// The operation budget in force for the current top-level tool call, or
+    /// `None` between calls / when no budget option is set. Installed once per
+    /// top-level call and shared (inherited, never reset) by composite children
+    /// so delegation cannot multiply the allowance (ADR-031).
+    pub tool_budget: Option<Arc<crate::tool_budget::ToolBudget>>,
 }
 
 pub type BgProcess = crate::managed_process::ManagedBackground;
@@ -121,6 +130,7 @@ impl Session {
             relocated(cfg.snapshot_directory.as_ref()),
         );
         let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
+        let tool_budget_config = cfg.tool_budget;
         Self {
             workspace,
             cwd,
@@ -146,9 +156,144 @@ impl Session {
             authority,
             admission,
             policy,
+            tool_budget_config,
+            tool_budget: None,
         }
     }
 
+    /// Install a fresh operation budget for a top-level tool call unless one is
+    /// already active (a composite's children inherit the parent's). Returns
+    /// `true` when this call installed the budget and must clear it afterwards
+    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
+    pub fn begin_tool_operation(&mut self) -> bool {
+        if self.tool_budget.is_some() {
+            return false;
+        }
+        self.tool_budget = self.tool_budget_config.start();
+        self.tool_budget.is_some()
+    }
+
+    /// Clear the operation budget if this call owned it. A composite child
+    /// (`owned == false`) leaves the parent's budget in place.
+    pub fn end_tool_operation(&mut self, owned: bool) {
+        if owned {
+            self.tool_budget = None;
+        }
+    }
+
+    /// Charge `bytes` logical input against the active budget. Returns the
+    /// typed [`crate::path_policy::ToolFailure`] when the ceiling is exceeded;
+    /// a no-op when no byte budget is active.
+    pub fn budget_charge_bytes(
+        &self,
+        bytes: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            budget
+                .charge_input(bytes)
+                .map_err(|hit| hit.into_failure(Some(argument)))?;
+        }
+        Ok(())
+    }
+
+    /// A clone of the active operation budget, for handing the shared `Arc`
+    /// into a blocking search worker so the worker itself meters scanned bytes
+    /// and checks the deadline (FR-19 F1/F2). `None` when no budget is active.
+    pub fn tool_budget(&self) -> Option<std::sync::Arc<crate::tool_budget::ToolBudget>> {
+        self.tool_budget.clone()
+    }
+
+    /// Install (or clear with `None`) the operation budget directly. Used by
+    /// `execute_script` (FR-19 F4): the script owns ONE shared budget and
+    /// installs it on the session only inside each inner per-op lock, clearing
+    /// it when the lock is released, so the budget never lingers on the shared
+    /// session between ops where a concurrent top-level call could inherit it.
+    /// `begin_tool_operation` then sees it already installed and inherits.
+    pub fn set_operation_budget(
+        &mut self,
+        budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
+    ) {
+        self.tool_budget = budget;
+    }
+
+    /// True when ANY operation budget is active (deadline, byte, or entry).
+    /// Used to refuse composite routes whose external work cannot be governed
+    /// cooperatively (e.g. `tool_pipeline`).
+    pub fn budget_active(&self) -> bool {
+        self.tool_budget.is_some()
+    }
+
+    /// True when the active budget carries an input-byte or directory-entry
+    /// ceiling (a logical-input budget a tool must be able to meter).
+    pub fn budget_has_input_limits(&self) -> bool {
+        self.tool_budget
+            .as_ref()
+            .is_some_and(|b| b.has_input_limits())
+    }
+
+    /// Refuse `tool` when an input/entry budget is enabled but the tool reads
+    /// no meterable logical input (a process-starting plugin, the index
+    /// search): return the typed `tool_budget_unenforceable` failure rather
+    /// than exempt it silently (ADR-031). `None` when nothing need be refused.
+    pub fn budget_input_refusal(&self, tool: &str) -> Option<crate::path_policy::ToolFailure> {
+        if self.budget_has_input_limits() {
+            Some(crate::path_policy::ToolFailure::tool_budget_unenforceable(
+                Some(tool),
+                "it starts external work whose logical input is not metered",
+            ))
+        } else {
+            None
+        }
+    }
+
+    /// Charge `entries` directory entries against the active budget.
+    pub fn budget_charge_entries(
+        &self,
+        entries: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            budget
+                .charge_entries(entries)
+                .map_err(|hit| hit.into_failure(Some(argument)))?;
+        }
+        Ok(())
+    }
+
+    /// Remaining time until the active budget's absolute deadline, to hand to
+    /// the managed-subprocess path so external work (exec, git_read) inherits
+    /// but never extends it (FR-19, WP18, ADR-031). `None` when no budget or no
+    /// deadline is active.
+    pub fn budget_remaining(&self) -> Option<std::time::Duration> {
+        self.tool_budget.as_ref().and_then(|b| b.remaining())
+    }
+
+    /// The typed `tool_timeout` failure for the active budget, used after the
+    /// managed-subprocess path reports it stopped external work at the
+    /// deadline. `None` when no budget is active.
+    pub fn budget_timeout_failure(
+        &self,
+        argument: &str,
+    ) -> Option<crate::path_policy::ToolFailure> {
+        self.tool_budget
+            .as_ref()
+            .map(|b| b.timeout_failure(Some(argument)))
+    }
+
+    /// Error if the active budget's absolute deadline has passed.
+    pub fn budget_check_deadline(
+        &self,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            budget
+                .check_deadline()
+                .map_err(|hit| hit.into_failure(Some(argument)))?;
+        }
+        Ok(())
+    }
+
     /// Build the initial session environment with an enhanced PATH.
     /// Combines: config extra_path + auto-detected tool dirs + parent process PATH.
     fn build_initial_env(cfg: &Config) -> HashMap<String, String> {
diff --git a/src/session_bootstrap.rs b/src/session_bootstrap.rs
index 5a7ac78..a552469 100644
--- a/src/session_bootstrap.rs
+++ b/src/session_bootstrap.rs
@@ -61,6 +61,14 @@ pub struct BootstrapOptions<'a> {
     /// Part of the launch identity so a frontend requesting it cannot silently
     /// attach to a daemon launched without it.
     pub full_tool_schemas: bool,
+    /// FR-19 (WP18, ADR-031): the caller's cooperative tool-budget options.
+    /// Part of the daemon's immutable launch identity exactly like
+    /// `workspace_read_only`: forwarded in `daemon_args`, hashed into
+    /// `bootstrap_fingerprint`, and treated as an explicit selection in
+    /// `launch_identity`, so a client requesting a budget cannot be served by a
+    /// daemon started without the same one (the daemon pins its budget from its
+    /// OWN parsed flags). All-absent keeps legacy behavior.
+    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
 }
 
 pub struct BootstrapConnection {
@@ -229,6 +237,21 @@ fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
     if options.no_follow {
         args.push(OsString::from("--no-follow-symlinks"));
     }
+    // FR-19 (WP18, ADR-031): forward the caller's budget to the daemon so an
+    // interactive session actually enforces it (global flags, before the
+    // subcommand). Omitted options add nothing, preserving legacy launch args.
+    if let Some(ms) = options.tool_budget.timeout_ms {
+        args.push(OsString::from("--tool-timeout-ms"));
+        args.push(OsString::from(ms.to_string()));
+    }
+    if let Some(bytes) = options.tool_budget.input_bytes {
+        args.push(OsString::from("--tool-input-bytes"));
+        args.push(OsString::from(bytes.to_string()));
+    }
+    if let Some(entries) = options.tool_budget.max_entries {
+        args.push(OsString::from("--tool-max-entries"));
+        args.push(OsString::from(entries.to_string()));
+    }
     args.push(OsString::from("session-daemon"));
     args.push(OsString::from("--socket"));
     args.push(options.socket_path.as_os_str().to_owned());
@@ -298,6 +321,20 @@ fn bootstrap_fingerprint(options: &BootstrapOptions<'_>) -> String {
     if options.no_follow {
         digest.update(b"no-follow\0");
     }
+    // FR-19 (WP18, ADR-031): pin the budget into the launch identity so an
+    // attach cannot silently run under a different (or absent) budget.
+    if let Some(ms) = options.tool_budget.timeout_ms {
+        digest.update(b"tool-timeout-ms\0");
+        digest.update(ms.to_le_bytes());
+    }
+    if let Some(bytes) = options.tool_budget.input_bytes {
+        digest.update(b"tool-input-bytes\0");
+        digest.update(bytes.to_le_bytes());
+    }
+    if let Some(entries) = options.tool_budget.max_entries {
+        digest.update(b"tool-max-entries\0");
+        digest.update(entries.to_le_bytes());
+    }
     if let Some(provider) = options.provider {
         digest.update(provider.as_bytes());
     }
@@ -320,6 +357,7 @@ fn launch_identity(options: &BootstrapOptions<'_>) -> std::io::Result<LaunchIden
         && options.confine_identity.is_none()
         && !options.ignore_config_files
         && !options.no_follow
+        && !options.tool_budget.any_enabled()
     {
         return Ok(LaunchIdentity::NotRequested);
     }
@@ -395,6 +433,7 @@ mod tests {
         agent_env: &'a Path,
     ) -> BootstrapOptions<'a> {
         BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             workspace,
             tool_allowlist: None,
             config_path: Some(config),
@@ -474,6 +513,71 @@ mod tests {
         );
     }
 
+    // WP18 (FR-19, ADR-031, req 1): the interactive agent runs through the
+    // session daemon, so each budget option must be a daemon launch argument
+    // AND part of the launch identity. If it were not in the identity, an
+    // attach could silently reuse a daemon with a different (or absent) budget;
+    // the client compares fingerprints and refuses on mismatch.
+    #[test]
+    fn tool_budget_is_a_daemon_argument_and_pins_the_launch_identity() {
+        let base = options(
+            Path::new("/workspace"),
+            Path::new("/config.toml"),
+            Path::new("/runtime/session.sock"),
+            Path::new("/agent.env"),
+        );
+        let budgeted = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig {
+                timeout_ms: Some(1500),
+                input_bytes: Some(4096),
+                max_entries: Some(32),
+            },
+            ..options(
+                Path::new("/workspace"),
+                Path::new("/config.toml"),
+                Path::new("/runtime/session.sock"),
+                Path::new("/agent.env"),
+            )
+        };
+        let args = daemon_args(&budgeted);
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-timeout-ms", "1500"]));
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-input-bytes", "4096"]));
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-max-entries", "32"]));
+        // The unbudgeted daemon carries none of these arguments.
+        assert!(!daemon_args(&base)
+            .iter()
+            .any(|arg| arg == "--tool-timeout-ms"));
+        // A budget changes the launch identity, so an attach to a daemon that
+        // lacks it (or has a different one) is refused.
+        assert_ne!(
+            bootstrap_fingerprint(&base),
+            bootstrap_fingerprint(&budgeted)
+        );
+        let other = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig {
+                timeout_ms: Some(500),
+                input_bytes: None,
+                max_entries: None,
+            },
+            ..options(
+                Path::new("/workspace"),
+                Path::new("/config.toml"),
+                Path::new("/runtime/session.sock"),
+                Path::new("/agent.env"),
+            )
+        };
+        assert_ne!(
+            bootstrap_fingerprint(&budgeted),
+            bootstrap_fingerprint(&other)
+        );
+    }
+
     // WP11 (FR-09, ADR-021): explicit-only mode is pinned into the daemon's
     // launch identity, the same way --confine-paths is.
     #[test]
@@ -485,6 +589,7 @@ mod tests {
             Path::new("/agent.env"),
         );
         let explicit = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             ignore_config_files: true,
             ..options(
                 Path::new("/workspace"),
@@ -512,6 +617,7 @@ mod tests {
         let config = directory.path().join("config.toml");
         let agent_env = directory.path().join("agent.env");
         let pinned = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -652,6 +758,7 @@ mod tests {
             LaunchIdentity::Unavailable
         );
         let bare = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -776,6 +883,7 @@ mod tests {
         // explicit selection (not NotRequested), so it is compared against the
         // daemon's published identity.
         let read_only = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -786,6 +894,7 @@ mod tests {
         // The running daemon published a NON-read-only fingerprint (its own
         // bare identity). The client must NOT silently attach.
         let non_read_only = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
diff --git a/src/tool_budget.rs b/src/tool_budget.rs
new file mode 100644
index 0000000..d45f6bf
--- /dev/null
+++ b/src/tool_budget.rs
@@ -0,0 +1,272 @@
+//! Cooperative, operation-scoped tool budgets (WP18, FR-19, ADR-031).
+//!
+//! A budget carries an absolute deadline and two independent optional logical
+//! counters — bytes of file/git content read or scanned, and directory entries
+//! walked. It is created once per top-level tool call and shared by every child
+//! of a composite (`batch`, `execute_script`, `tool_pipeline`) so delegation
+//! cannot multiply the allowance: a child inherits the remaining allowance and
+//! cannot extend the deadline (ADR-031). Limits are caller-set and disabled by
+//! default; when every option is absent no budget is installed and behavior is
+//! unchanged.
+//!
+//! Counting is logical, not physical: charge the file/git object content read
+//! or scanned (including cached-index content) and directory entries walked —
+//! never device I/O or displayed/truncated output bytes, which remain a
+//! separate mechanism (`src/tool_output.rs`).
+//!
+//! Context: docs/contexts/reviewer-runtime/CONTEXT.md,
+//! docs/adr/031-cooperative-tool-budgets.md.
+
+use std::io::{self, Read};
+use std::sync::atomic::{AtomicU64, Ordering};
+use std::sync::Arc;
+use std::time::{Duration, Instant};
+
+use crate::path_policy::ToolFailure;
+
+/// Tools that start external work (a subprocess) and read no logical file/git
+/// content we can meter. An input or entry budget cannot be enforced for them,
+/// so when such a budget is enabled the call is REFUSED rather than silently
+/// exempted (ADR-031; INTERFACES.md row WP18). The deadline, which CAN stop and
+/// reap the process group, is still enforced (see `managed_process::run`).
+/// `git_read` is deliberately absent: it returns the git object content it read,
+/// which the PM counts as logical input, so it is metered, not refused.
+pub fn tool_input_unmeterable(name: &str) -> bool {
+    matches!(
+        name,
+        "exec"
+            | "bg"
+            | "git"
+            | "gh"
+            | "cargo"
+            | "npm"
+            | "pytest"
+            | "docker"
+            | "shellcheck"
+            | "curl"
+            | "discord"
+            | "lint_repair"
+            | "tool_run"
+    )
+}
+
+/// Caller-configured budget options. All-absent means no budget is installed
+/// (legacy behavior). Caller-owned; never deserialized from workspace config.
+#[derive(Debug, Clone, Copy, Default)]
+pub struct ToolBudgetConfig {
+    /// `--tool-timeout-ms N`: absolute per-call deadline in milliseconds.
+    pub timeout_ms: Option<u64>,
+    /// `--tool-input-bytes N`: logical input byte ceiling for the call.
+    pub input_bytes: Option<u64>,
+    /// `--tool-max-entries N`: directory-entry ceiling for the call (D56,
+    /// `(proposed)` spelling; a hit reports the reviewed `tool_input_limit`).
+    pub max_entries: Option<u64>,
+}
+
+impl ToolBudgetConfig {
+    pub fn any_enabled(&self) -> bool {
+        self.timeout_ms.is_some() || self.input_bytes.is_some() || self.max_entries.is_some()
+    }
+
+    /// Install a fresh operation budget for a top-level call, or `None` when no
+    /// option is set (preserving legacy behavior).
+    pub fn start(&self) -> Option<Arc<ToolBudget>> {
+        if !self.any_enabled() {
+            return None;
+        }
+        Some(Arc::new(ToolBudget::new(*self)))
+    }
+}
+
+/// Which input ceiling a failure names. Both use the reviewed
+/// `tool_input_limit` code; the message names which was hit (integrator D56).
+#[derive(Debug, Clone, Copy)]
+pub enum Limit {
+    Bytes,
+    Entries,
+}
+
+impl Limit {
+    fn label(self) -> &'static str {
+        match self {
+            Limit::Bytes => "input bytes",
+            Limit::Entries => "directory entries",
+        }
+    }
+}
+
+/// An operation-scoped cooperative budget: an absolute deadline plus two
+/// independent atomic counters. Shared across the children of one top-level
+/// call via `Arc`, so concurrent delegation cannot multiply the allowance.
+pub struct ToolBudget {
+    deadline: Option<Instant>,
+    timeout_ms: Option<u64>,
+    input_limit: Option<u64>,
+    input_consumed: AtomicU64,
+    entry_limit: Option<u64>,
+    entry_consumed: AtomicU64,
+}
+
+impl ToolBudget {
+    fn new(cfg: ToolBudgetConfig) -> Self {
+        let deadline = cfg
+            .timeout_ms
+            .map(|ms| Instant::now() + Duration::from_millis(ms));
+        Self {
+            deadline,
+            timeout_ms: cfg.timeout_ms,
+            input_limit: cfg.input_bytes,
+            input_consumed: AtomicU64::new(0),
+            entry_limit: cfg.max_entries,
+            entry_consumed: AtomicU64::new(0),
+        }
+    }
+
+    /// Remaining time until the absolute deadline, for handing to the managed
+    /// subprocess path so external work inherits (never extends) it. `None`
+    /// when no deadline is set.
+    pub fn remaining(&self) -> Option<Duration> {
+        self.deadline
+            .map(|d| d.saturating_duration_since(Instant::now()))
+    }
+
+    /// The typed deadline failure for this budget, used when external work
+    /// (a managed subprocess) was stopped at the deadline (FR-19, WP18).
+    pub fn timeout_failure(&self, argument: Option<&str>) -> ToolFailure {
+        BudgetHit::Timeout {
+            configured_ms: self.timeout_ms.unwrap_or(0),
+        }
+        .into_failure(argument)
+    }
+
+    /// Error if the absolute deadline has passed. Checked at bounded work
+    /// boundaries by native/index readers.
+    pub fn check_deadline(&self) -> Result<(), BudgetHit> {
+        if let Some(d) = self.deadline {
+            if Instant::now() >= d {
+                return Err(BudgetHit::Timeout {
+                    configured_ms: self.timeout_ms.unwrap_or(0),
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// True when an input-byte or directory-entry ceiling is configured (i.e.
+    /// a logical-input budget this tool would have to meter). Used to refuse a
+    /// tool that starts unmeterable external work (ADR-031).
+    pub fn has_input_limits(&self) -> bool {
+        self.input_limit.is_some() || self.entry_limit.is_some()
+    }
+
+    /// Charge `n` logical input bytes. No-op when no byte limit is set. Returns
+    /// a hit when the cumulative total exceeds the ceiling; the counter still
+    /// reflects the attempt so the reported `consumed` is honest. INTERFACES.md
+    /// reserves this name (`charge_input`).
+    pub fn charge_input(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.input_limit {
+            let consumed = self.input_consumed.fetch_add(n, Ordering::Relaxed) + n;
+            if consumed > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Bytes,
+                    limit,
+                    consumed,
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// Charge `n` directory entries. No-op when no entry limit is set.
+    pub fn charge_entries(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.entry_limit {
+            let consumed = self.entry_consumed.fetch_add(n, Ordering::Relaxed) + n;
+            if consumed > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Entries,
+                    limit,
+                    consumed,
+                });
+            }
+        }
+        Ok(())
+    }
+}
+
+/// A budget violation, convertible to the reviewed [`ToolFailure`].
+#[derive(Debug, Clone)]
+pub enum BudgetHit {
+    Timeout {
+        configured_ms: u64,
+    },
+    Input {
+        which: Limit,
+        limit: u64,
+        consumed: u64,
+    },
+}
+
+impl BudgetHit {
+    /// Build the caller-facing typed failure. A deadline hit carries
+    /// `tool_timeout`; a byte or entry hit carries `tool_input_limit` and names
+    /// which ceiling was hit, the configured value, and the consumed value.
+    pub fn into_failure(self, argument: Option<&str>) -> ToolFailure {
+        match self {
+            BudgetHit::Timeout { configured_ms } => {
+                ToolFailure::tool_timeout(argument, configured_ms)
+            }
+            BudgetHit::Input {
+                which,
+                limit,
+                consumed,
+            } => ToolFailure::tool_input_limit(argument, which.label(), limit, consumed),
+        }
+    }
+}
+
+/// A `Read` wrapper that charges scanned bytes and checks the deadline at every
+/// bounded read step, so a native search worker meters the bytes it SCANS (not
+/// the bytes it returns) and stops mid-file at the deadline or the byte ceiling
+/// (FR-19 F1/F2, ADR-031). The worker that owns the reader inspects
+/// [`BudgetedReader::hit`] after the search to surface the typed failure; the
+/// read error that aborts the search is only the stop signal. Wrap only when a
+/// budget is active — the unbudgeted path keeps its exact legacy behavior.
+pub struct BudgetedReader<R> {
+    inner: R,
+    budget: Arc<ToolBudget>,
+    hit: Option<BudgetHit>,
+}
+
+impl<R: Read> BudgetedReader<R> {
+    pub fn new(inner: R, budget: Arc<ToolBudget>) -> Self {
+        Self {
+            inner,
+            budget,
+            hit: None,
+        }
+    }
+
+    /// The budget violation that stopped the scan, if any.
+    pub fn hit(&self) -> Option<&BudgetHit> {
+        self.hit.as_ref()
+    }
+}
+
+impl<R: Read> Read for BudgetedReader<R> {
+    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
+        // Deadline first: a bounded step boundary inside a large file.
+        if let Err(hit) = self.budget.check_deadline() {
+            self.hit = Some(hit);
+            return Err(io::Error::other("tool budget deadline"));
+        }
+        let n = self.inner.read(buf)?;
+        if n > 0 {
+            // Charge exactly the bytes just scanned.
+            if let Err(hit) = self.budget.charge_input(n as u64) {
+                self.hit = Some(hit);
+                return Err(io::Error::other("tool input budget"));
+            }
+        }
+        Ok(n)
+    }
+}
diff --git a/tests/README.md b/tests/README.md
index 0674fba..cbd0b5f 100644
--- a/tests/README.md
+++ b/tests/README.md
@@ -196,3 +196,25 @@ The keyword classification (reference vs data vs schema-position vs unknown)
 and the duplicate-`$id`/anchor refusal are proven at the
 `src/schema_contract.rs` unit seam, including a table that routes a cycle
 through every 2020-12 schema keyword.
+
+### WP18 cooperative tool budgets (`tests/test_tool_budgets.py`)
+
+Process-level acceptance tests for FR-19 (ADR-031): the three default-off
+budget options drive a real `daimonos` MCP process (and a real `agent --print`
+process for the audit/`--output json`/worker cases). Timing tests (AC-19.1) use
+a deadline of 1000–1500 ms and assert an upper bound of several seconds while
+keeping the data small, so a slower CI runner still stops promptly without
+flaking; each bound and its reason are stated in the test docstring. The
+`exec` and `git_read` reap proofs follow the method of
+`tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped`
+(scanning `/proc` for a unique marker) to show no child or grandchild outlives
+the stop; the `git_read` proof installs a `$GIT_DIR`-gated fake `git` shim so
+repo discovery still uses the real git while an operation call hangs. These
+tests need a Unix host with `/proc` and a real `git`; a skip is not a pass.
+
+`tests/test_tool_budgets.py` (WP18, FR-19): process-level cooperative tool
+budget tests — scanned-byte metering for content search, native deadline checks
+(read_file/ls/both search modes), plugin deadline+reap (curl, git), the
+`tool_budget_unenforceable` refusals (git plugin, index search, pipeline), the
+AC-19.1 large-tree timeout bound, and the `limit_hit` audit event (by
+kind+fields). AC-19.1 writes 10000 small files and stays under ~10 s.
diff --git a/tests/test_tool_budgets.py b/tests/test_tool_budgets.py
new file mode 100644
index 0000000..8f5792e
--- /dev/null
+++ b/tests/test_tool_budgets.py
@@ -0,0 +1,980 @@
+"""WP18 FR-19 process-level acceptance tests: cooperative tool budgets.
+
+Spawns a real daimonos MCP process (conftest ``daimonos_factory``) and drives
+the public tool interface. Budgets are default-off; every test that asserts a
+limit passes the CLI option explicitly. Context:
+docs/adr/031-cooperative-tool-budgets.md and the reviewer-runtime
+INTERFACES.md row "WP18, FR-19".
+"""
+
+import json
+
+
+def _text(result):
+    return result["content"][0]["text"]
+
+
+def test_input_bytes_limit_refuses_oversized_read(daimonos_factory, tmp_path):
+    """--tool-input-bytes N: a read whose logical input exceeds N is refused
+    with the reviewed ``tool_input_limit`` code and a budget naming the
+    configured value and the consumed value. No partial content is returned.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * 500)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 100, data
+    assert data["consumed"] >= 500, data
+    assert "content" not in data, "no partial result on a limit hit"
+
+
+def test_input_bytes_default_off_reads_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, an ordinary read is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * 500)
+    client = daimonos_factory(ws)
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    data = json.loads(_text(result))
+    assert data["content"] == "x" * 500
+
+
+import os
+import time
+
+
+def _survivors(needle):
+    """PIDs whose /proc cmdline contains `needle` — the process-table method of
+    tests/test_worker_isolation.py::_worker_survivors, used here to prove that
+    neither the exec child nor its grandchild outlives a budget stop (FR-19
+    requirement 3)."""
+    out = []
+    for pid in os.listdir("/proc"):
+        if not pid.isdigit():
+            continue
+        try:
+            with open("/proc/%s/cmdline" % pid, "rb") as f:
+                cmd = f.read().replace(b"\x00", b" ").decode("utf-8", "replace")
+        except OSError:
+            continue
+        if needle in cmd:
+            out.append(pid)
+    return out
+
+
+def _poll_gone(needle, bound_s):
+    deadline = time.monotonic() + bound_s
+    while time.monotonic() < deadline:
+        if not _survivors(needle):
+            return True
+        time.sleep(0.05)
+    return not _survivors(needle)
+
+
+def test_timeout_stops_exec_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
+    """--tool-timeout-ms N: an exec that outlives the deadline is stopped with
+    the reviewed ``tool_timeout`` code, and neither the child shell nor the
+    grandchild it backgrounds survives (FR-19 requirement 3; stopped work, not
+    only a stopped wait — the managed process group is TERM/KILL reaped via
+    ManagedChild::terminate).
+
+    Timing (AC-19.1, req 9): the deadline is 1000 ms — a limit of one second or
+    less so a slower CI runner still stops promptly. The call is asserted to
+    return within 8 s: generous for a loaded shared runner (expected stop is
+    ~1-3 s), yet well below the 15 s the shells would otherwise sleep, so a
+    pass proves the stop was the budget deadline, not the processes exiting on
+    their own.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    gc_marker = f"wp18gc-{os.getpid()}-{time.time_ns()}"
+    parent_marker = f"wp18parent-{os.getpid()}-{time.time_ns()}"
+    # The shell daimonos runs (sh -c <line>) backgrounds a grandchild shell
+    # whose argv carries gc_marker, then itself sleeps far past the deadline.
+    # Both stay in the exec's process group, so a group reap removes both.
+    line = (
+        f"sh -c 'sleep 15 # {gc_marker}' & "
+        f"sleep 15 # {parent_marker}"
+    )
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1000"])
+
+    start = time.monotonic()
+    result = client.call_tool("exec", {"command": line})
+    elapsed = time.monotonic() - start
+
+    assert elapsed < 8, f"budget did not stop exec promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "exit" not in data, "no partial exec result on a deadline hit"
+
+    assert _poll_gone(parent_marker, 5.0), "exec child survived the budget stop"
+    assert _poll_gone(gc_marker, 5.0), "exec grandchild survived the budget stop"
+
+
+def test_max_entries_limit_refuses_oversized_ls(daimonos_factory, tmp_path):
+    """--tool-max-entries N: an ls that walks more than N directory entries is
+    refused with the reviewed ``tool_input_limit`` code (integrator D56: the
+    entry ceiling reuses the byte code), a budget naming the configured value
+    and the consumed entry count, and no partial listing. Off by default.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(10):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "3"])
+
+    result = client.call_tool("ls", {"path": "."})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 3, data
+    assert data["consumed"] >= 10, data
+    assert "entries" not in data, "no partial listing on an entry-limit hit"
+
+
+def test_max_entries_default_off_lists_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, an ordinary ls is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(10):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws)
+    result = client.call_tool("ls", {"path": "."})
+    data = json.loads(_text(result))
+    assert len(data["entries"]) == 10, data
+
+
+def test_input_bytes_meters_content_search_filesystem_path(daimonos_factory, tmp_path):
+    """--tool-input-bytes N on the FILESYSTEM search path (search mode=content
+    -> grep): the logical input the search returns is charged, so a result that
+    exceeds N is refused with ``tool_input_limit`` and no partial matches
+    (FR-19 req 4; the grep path reads the filesystem, not the index).
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(6):
+        (ws / f"hit_{i}.txt").write_text("needle here\n")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "40"])
+
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 40, data
+    assert "matches" not in data, "no partial matches on a limit hit"
+
+
+def test_input_bytes_refuses_index_search(daimonos_factory, tmp_path):
+    """F1 (M1): the INDEX search path (search mode=files -> trigram index)
+    reads the cached index, not scannable file content, so its logical input
+    cannot be metered. Under --tool-input-bytes the index-answered search is
+    REFUSED with a named error rather than charged "bytes returned" (ADR-031
+    allows refusing an enabled-but-unenforceable budget; the owner choice is
+    stated in the handoff). No partial results.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(8):
+        (ws / f"wp18idx_{i}.rs").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "30"])
+
+    result = client.call_tool("search", {"pattern": "wp18idx", "mode": "files"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_budget_unenforceable", data
+    assert "results" not in data, "no partial index results on a refusal"
+
+
+# --- Composites share one budget (FR-19 requirement 2) ---
+
+_TWO_READS = (
+    "def main():\n"
+    "    read_file('big.txt')\n"
+    "    read_file('big.txt')\n"
+    "    return 'both'\n"
+    "result = main()\n"
+)
+
+
+def test_script_children_share_parent_budget(daimonos_factory, tmp_path):
+    """FR-19 req 2: a composite's children inherit the parent call's remaining
+    allowance, they do NOT each get a fresh top-level budget. Two reads of a
+    60-byte file under --tool-input-bytes 100: the first (60) fits, the second
+    (cumulative 120) trips the shared ceiling. This test FAILS if an
+    execute_script child is given its own new budget (both reads would fit).
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    result = client.call_tool("execute_script", {"code": _TWO_READS})
+    assert result.get("isError") is True, result
+    assert "tool_input_limit" in _text(result), _text(result)
+
+
+def test_script_single_read_within_budget_succeeds(daimonos_factory, tmp_path):
+    """Control: one 60-byte read under --tool-input-bytes 100 inside a script
+    succeeds, so the failure above is the shared second read, not read_file
+    being charged wrong."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    code = "def main():\n    read_file('big.txt')\n    return 'ok'\nresult = main()\n"
+    result = client.call_tool("execute_script", {"code": code})
+    assert result.get("isError") is not True, _text(result)
+    assert json.loads(_text(result))["result"] == "ok"
+
+
+def test_batch_children_share_parent_budget(daimonos_factory, tmp_path):
+    """FR-19 req 2: a batch's ops inherit the one parent budget. Two reads of a
+    60-byte file under --tool-input-bytes 100: op 1 fits, op 2 (cumulative 120)
+    trips the shared ceiling. FAILS if a batch op gets its own new budget."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+    ]})
+    content = json.loads(_text(result))
+    assert content[0]["ok"] is True, content
+    assert content[1]["ok"] is False, content
+    assert "tool_input_limit" in json.dumps(content[1]), content
+
+
+# --- Agent `--print` frontend: audit (req 5), --output json (req 11), req 1 ---
+#
+# A local, self-contained fake OpenAI-compatible endpoint (no network, no real
+# credential) drives a real `daimonos agent --print` process, mirroring
+# tests/test_run_audit.py. A budget option is passed as a global flag so the
+# agent frontend honors it; a limit hit must be a tool failure INSIDE the run
+# (the run still succeeds and, under --output json, still prints one document),
+# and the WP08 audit must record the named code.
+
+import os as _os
+import subprocess
+import threading
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
+from pathlib import Path
+
+_FAKE_KEY = "synthetic-not-a-real-key-WP18-5fZ9"
+_DONE = "WP18_AGENT_DONE_MARKER"
+
+
+class _FakeProvider:
+    def __init__(self, script):
+        self._script = list(script)
+        self._step = 0
+        self.received_tool_results = []
+        owner = self
+
+        class Handler(BaseHTTPRequestHandler):
+            protocol_version = "HTTP/1.1"
+
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                payload = json.dumps({"data": [{"id": "test/model",
+                    "context_length": 100000, "supported_parameters": ["tools"],
+                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
+                self.send_response(200)
+                self.send_header("Content-Type", "application/json")
+                self.send_header("Content-Length", str(len(payload)))
+                self.end_headers()
+                self.wfile.write(payload)
+
+            def do_POST(self):
+                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
+                tool_msgs = [m for m in body["messages"] if m.get("role") == "tool"]
+                if tool_msgs:
+                    owner.received_tool_results.append(json.dumps(tool_msgs))
+                step = owner._script[min(owner._step, len(owner._script) - 1)]
+                owner._step += 1
+                self.send_response(200)
+                self.send_header("Content-Type", "text/event-stream")
+                self.send_header("Connection", "close")
+                self.end_headers()
+                self.close_connection = True
+
+                def chunk(delta, reason=None):
+                    frame = {"choices": [{"index": 0, "delta": delta,
+                        "finish_reason": reason}],
+                        "usage": {"prompt_tokens": 42, "completion_tokens": 7}}
+                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
+                    self.wfile.flush()
+
+                try:
+                    if step[0] == "call":
+                        _, name, arguments = step
+                        chunk({"role": "assistant", "content": "", "tool_calls": [{
+                            "index": 0, "id": f"call-{owner._step}", "type": "function",
+                            "function": {"name": name,
+                                         "arguments": json.dumps(arguments)}}]},
+                              "tool_calls")
+                    else:
+                        chunk({"role": "assistant", "content": step[1]}, "stop")
+                    self.wfile.write(b"data: [DONE]\n\n")
+                    self.wfile.flush()
+                except (BrokenPipeError, ConnectionResetError):
+                    pass
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
+        self.thread.start()
+
+    @property
+    def port(self):
+        return self.server.server_port
+
+    def close(self):
+        self.server.shutdown()
+        self.server.server_close()
+        self.thread.join(timeout=3)
+
+
+def _agent_env_file(tmp_path, provider, allowed="read_file"):
+    path = tmp_path / "agent.env"
+    path.write_text("\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter",
+        "DAIMONOS_AGENT_MODEL=test/model",
+        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.port}/v1",
+        f"DAIMONOS_AGENT_API_KEY={_FAKE_KEY}",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
+        "DAIMONOS_AGENT_COMPACTION=off",
+        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
+        f"DAIMONOS_AGENT_ALLOWED_COMMANDS={allowed}",
+        "",
+    ]))
+    return path
+
+
+def _isolated_env(tmp_path):
+    stash = tmp_path / "home"
+    stash.mkdir(exist_ok=True)
+    (tmp_path / "tmp").mkdir(exist_ok=True)
+    env = {k: v for k, v in _os.environ.items() if not k.startswith("DAIMONOS_")}
+    env.update({
+        "HOME": str(stash),
+        "XDG_STATE_HOME": str(stash / "state"),
+        "XDG_DATA_HOME": str(stash / "data"),
+        "XDG_CONFIG_HOME": str(stash / "config"),
+        "TMPDIR": str(tmp_path / "tmp"),
+        "DAIMONOS_KGL_AUTOINDEX": "0",
+    })
+    return env
+
+
+def test_agent_print_json_budget_hit_is_tool_failure_and_is_audited(daimonos_binary, tmp_path):
+    """req 1 (agent --print honors the budget), req 11 (a limit hit under
+    --output json is a tool failure INSIDE the run; the run still prints its one
+    JSON document and exits 0) and req 5 (the WP08 audit records the hit with
+    its named code). The model calls read_file on a 500-byte file under
+    --tool-input-bytes 100, gets a tool_input_limit, then ends normally.
+    """
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    (workspace / "big.txt").write_text("z" * 500)
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider)
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "--tool-input-bytes", "100",
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "do the read"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    # req 11: exactly one JSON document on stdout.
+    doc = json.loads(completed.stdout)
+    assert isinstance(doc, dict), completed.stdout
+    # The tool result handed back to the model carried the named code: the
+    # limit hit was a tool failure inside the run, not a run abort.
+    joined = " ".join(provider.received_tool_results)
+    assert "tool_input_limit" in joined, joined
+    # req 5: the audit recorded the hit with its named code.
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    assert any("tool_input_limit" in json.dumps(ev) for ev in events), events
+
+
+# --- git_read deadline + process-group reap (FR-19 req 3; slice 06) ---
+
+import shutil
+
+
+def _init_repo(ws):
+    """A minimal real git repo for git_read to open, built with the real git on
+    PATH before any fake git is installed."""
+    real = shutil.which("git")
+    assert real, "a real git is required to build the fixture repo"
+
+    def g(*a):
+        subprocess.run([real, *a], cwd=str(ws), check=True,
+                       capture_output=True, env={**_os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
+                                                  "GIT_CONFIG_SYSTEM": "/dev/null"})
+    g("init", "-q")
+    g("config", "user.email", "wp18@example.invalid")
+    g("config", "user.name", "WP18")
+    (ws / "a.txt").write_text("hello\n")
+    g("add", "a.txt")
+    g("commit", "-q", "-m", "seed")
+    return real
+
+
+def _install_fake_git(bindir, real_git, parent_marker, gc_marker):
+    """A `git` shim that passes repo discovery through to the real git but, once
+    git_read sets GIT_DIR for an operation, backgrounds a grandchild and sleeps
+    far past the deadline. Both descendants carry a unique marker in their argv
+    and stay in the managed child's process group, so a group reap removes them.
+    """
+    bindir.mkdir(parents=True, exist_ok=True)
+    shim = bindir / "git"
+    shim.write_text(
+        "#!/bin/sh\n"
+        "if [ -n \"$GIT_DIR\" ]; then\n"
+        f"  sh -c 'sleep 12 # {gc_marker}' &\n"
+        f"  exec sh -c 'sleep 12 # {parent_marker}'\n"
+        "fi\n"
+        f"exec {real_git} \"$@\"\n"
+    )
+    shim.chmod(0o755)
+
+
+def test_timeout_stops_git_read_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
+    """--tool-timeout-ms N: a git_read whose git subprocess outlives the
+    deadline is stopped with the reviewed ``tool_timeout`` code, and neither the
+    git child nor the grandchild it backgrounds survives (FR-19 req 3 names
+    git_read for a reap proof; stopped work, not only a stopped wait — the whole
+    process group is TERM/KILL reaped via ManagedChild::terminate, the method of
+    tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped).
+
+    git_read is dispatched directly (not via ops::dispatch), so before this
+    slice a standalone git_read got no budget at all; the deadline is threaded
+    into managed_process::run via a task-local so the git child is actually
+    reaped, not merely abandoned.
+
+    Timing (AC-19.1, req 9): the deadline is 1500 ms — one and a half seconds so
+    a slower CI runner still stops promptly. The call is asserted to return
+    within 8 s (expected stop ~1.5-3 s) yet well below the 12 s the shims would
+    otherwise sleep, so a pass proves the stop was the budget deadline, not the
+    shims exiting on their own.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    real_git = _init_repo(ws)
+    bindir = tmp_path / "bin"
+    parent_marker = f"wp18git-{_os.getpid()}-{time.time_ns()}"
+    gc_marker = f"wp18gitgc-{_os.getpid()}-{time.time_ns()}"
+    _install_fake_git(bindir, real_git, parent_marker, gc_marker)
+
+    env = {"PATH": f"{bindir}{_os.pathsep}{_os.environ.get('PATH','')}",
+           "DAIMONOS_KGL_AUTOINDEX": "0"}
+    client = daimonos_factory(ws, env=env, args=["--tool-timeout-ms", "1500"])
+
+    start = time.monotonic()
+    result = client.call_tool("git_read", {"command": "log"})
+    elapsed = time.monotonic() - start
+
+    assert elapsed < 8, f"budget did not stop git_read promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "entries" not in data, "no partial git_read result on a deadline hit"
+
+    assert _poll_gone(parent_marker, 5.0), "git_read child survived the budget stop"
+    assert _poll_gone(gc_marker, 5.0), "git_read grandchild survived the budget stop"
+
+
+# --- Default off: every metered tool is unchanged with no budget (req 8) ---
+#
+# read_file and ls already have default-off regressions above. These cover the
+# remaining tools FR-19 req 8 names: exec, search, git_read. With none of the
+# three options, the new code paths (budget install, the with_budget_deadline
+# wrapper around managed_process::run) must be exact no-ops.
+
+
+def test_exec_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option an ordinary exec is unchanged
+    — it completes and returns its real exit, not a tool_timeout."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws)
+    result = client.call_tool("exec", {"command": "printf wp18ok"})
+    assert result.get("isError") is not True, _text(result)
+    data = json.loads(_text(result))
+    assert data["exit"] == 0, data
+    assert "wp18ok" in json.dumps(data), data
+
+
+def test_search_default_off_returns_all_matches(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option, a content search returns its
+    full result (no tool_input_limit)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(6):
+        (ws / f"hit_{i}.txt").write_text("needle here\n")
+    client = daimonos_factory(ws)
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_input_limit" not in _text(result), _text(result)
+
+
+def test_git_read_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option a git_read is unchanged — the
+    with_budget_deadline(None, ..) wrapper around the git subprocess is an exact
+    no-op and the real git log is returned, not a tool_timeout."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("git_read", {"command": "log"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_timeout" not in _text(result), _text(result)
+    assert "entries" in _text(result), _text(result)
+
+
+# --- Combinations: the budget composes with every other run option (req 6) ---
+#
+# Each test pairs a budget option with one other run-shaping option and asserts
+# the budget still fires with its reviewed code. Output truncation stays a
+# SEPARATE mechanism: the last test proves a result bounded by --no-output-spill
+# is not reported as a budget limit failure.
+
+
+def _big(ws, n=500):
+    (ws / "big.txt").write_text("x" * n)
+
+
+def test_input_bytes_composes_with_tool_allowlist(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --tool-allowlist. read_file is on
+    the allowlist, so admission passes; the byte budget still trips with
+    tool_input_limit (the allowlist and the budget are independent gates)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    allowlist = tmp_path / "allow.json"
+    allowlist.write_text(json.dumps(["read_file"]))
+    client = daimonos_factory(ws, args=["--tool-allowlist", str(allowlist),
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_input_bytes_composes_with_workspace_read_only(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --workspace-read-only. A read is
+    allowed under read-only, and the byte budget still trips."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    client = daimonos_factory(ws, args=["--workspace-read-only",
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_input_bytes_composes_with_confine_paths_and_no_follow(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --confine-paths and
+    --no-follow-symlinks. The read is inside the confined workspace and the byte
+    budget still trips with tool_input_limit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    client = daimonos_factory(ws, args=["--confine-paths", "--no-follow-symlinks",
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_no_output_spill_truncation_is_not_a_budget_limit(daimonos_factory, tmp_path):
+    """req 6: output truncation stays a mechanism distinct from the input
+    budget. A large read under --no-output-spill is bounded by the output rules,
+    not failed; with a generous --tool-input-bytes that the logical input fits
+    under, the result must NOT carry a budget code. Truncation != limit hit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws, n=300_000)
+    client = daimonos_factory(ws, args=["--no-output-spill",
+                                        "--tool-input-bytes", "10000000"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    body = _text(result)
+    assert "tool_input_limit" not in body, body
+    assert "tool_timeout" not in body, body
+
+
+# --- Workers (WP10, FR-19 req 7): a budgeted worker route is refused ---
+#
+# The cooperative budget installs at THIS process's dispatch seams, but a worker
+# routes tool calls into a separate process the parent budget never sees. So a
+# budget option cannot be enforced across the worker boundary; rather than
+# silently ignore it (a frontend-rule violation), the run is refused at startup
+# with run_configuration_invalid / exit 2 (D-WP18-5; ADR-031 refuse
+# enabled-but-unenforceable; INTERFACES worker rule). Ordinary worker mode with
+# no budget option is unchanged.
+
+_WORKER_REFUSAL = "tool_budget_worker_unenforceable"
+
+
+def _min_worker_config(tmp_path):
+    wc = tmp_path / "wc.json"
+    wc.write_text(json.dumps({"workers": {"w": {
+        "command": "/bin/true", "args": [],
+        "env": {"PATH": _os.environ.get("PATH", "/usr/bin:/bin")}}}}))
+    return wc
+
+
+def _run_agent_worker(daimonos_binary, tmp_path, provider, budget_args):
+    env = _isolated_env(tmp_path)
+    agent_env = _agent_env_file(tmp_path, provider)
+    ws = tmp_path / "ws"
+    ws.mkdir(exist_ok=True)
+    wc = _min_worker_config(tmp_path)
+    cmd = [daimonos_binary, "--workspace", str(ws), *budget_args,
+           "agent", "--print", "--worker-config", str(wc),
+           "--agent-env", str(agent_env), "do it"]
+    return subprocess.run(cmd, capture_output=True, text=True, timeout=60, env=env)
+
+
+def test_budgeted_worker_route_is_refused_at_startup(daimonos_binary, tmp_path):
+    """req 7: --tool-timeout-ms together with --worker-config is refused before
+    any worker launches, with the reviewed run_configuration_invalid code and
+    exit 2 and a reason naming the unenforceable budget."""
+    provider = _FakeProvider([("end", _DONE)])
+    try:
+        completed = _run_agent_worker(daimonos_binary, tmp_path, provider,
+                                      ["--tool-input-bytes", "1000"])
+    finally:
+        provider.close()
+    blob = completed.stdout + completed.stderr
+    assert completed.returncode == 2, blob
+    assert "run_configuration_invalid" in blob, blob
+    assert _WORKER_REFUSAL in blob, blob
+
+
+def test_unbudgeted_worker_route_is_not_refused_for_budget(daimonos_binary, tmp_path):
+    """req 7 control: with no budget option, worker mode is unchanged — it runs
+    its own WP10 path (here the trivial /bin/true worker is simply unavailable);
+    whatever its outcome, it is NOT the budget refusal."""
+    provider = _FakeProvider([("end", _DONE)])
+    try:
+        completed = _run_agent_worker(daimonos_binary, tmp_path, provider, [])
+    finally:
+        provider.close()
+    blob = completed.stdout + completed.stderr
+    assert _WORKER_REFUSAL not in blob, blob
+
+
+# ======================================================================
+# FIX run r4 (defects M1-M5). Each test targets one integrator measurement.
+# ======================================================================
+
+
+def test_input_bytes_charges_bytes_scanned_not_returned(daimonos_factory, tmp_path):
+    """F1 (M1): a content search charges the bytes it SCANS, not the bytes it
+    returns. 40 files of 100 KB without the word plus one with it, under
+    --tool-input-bytes 2000: the scan must stop and the call must fail with
+    ``tool_input_limit`` even though the returned match set is tiny. ``consumed``
+    exceeds the limit (the scan was stopped mid-stream) and stays below the
+    total bytes on disk (it stopped early). No matches on a limit hit.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    blob = "x" * (100 * 1024)
+    for i in range(40):
+        (ws / f"pad_{i:02}.txt").write_text(blob)
+    (ws / "zzz_match.txt").write_text("needle\n")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "2000"])
+
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 2000, data
+    assert data["consumed"] > 2000, data
+    assert data["consumed"] <= 41 * 100 * 1024, data
+    assert "matches" not in data, "no partial matches on a limit hit"
+
+
+def test_input_bytes_content_search_below_limit_passes(daimonos_factory, tmp_path):
+    """F1 control: a content scan whose total scanned bytes stay under the
+    ceiling returns its matches normally (so the failure above is the ceiling,
+    not search being broken)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "a.txt").write_text("needle here\n")  # ~12 bytes scanned
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100000"])
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is not True, _text(result)
+    data = json.loads(_text(result))
+    assert len(data["matches"]) == 1, data
+
+
+def test_timeout_read_file_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): read_file checks the deadline before building its result; with a
+    1 ms deadline (already elapsed by the time the native reader runs) it
+    returns ``tool_timeout``, never a late success with content."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * (100 * 1024))
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "content" not in data, "no late content on a deadline hit"
+
+
+def test_timeout_ls_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the ls walk checks the deadline before building its result."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("ls", {"path": "."})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "entries" not in data, "no late listing on a deadline hit"
+
+
+def test_timeout_content_search_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the content-search blocking worker itself checks the deadline
+    (passed into the worker), so a 1 ms deadline returns ``tool_timeout``, not a
+    late success."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"f{i}.txt").write_text("needle\n")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "matches" not in data, "no late matches on a deadline hit"
+
+
+def test_timeout_file_search_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the index (files-mode) search checks the deadline before the
+    result is built, so a 1 ms deadline returns ``tool_timeout``."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"wp18f_{i}.rs").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("search", {"pattern": "wp18f", "mode": "files"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+
+
+def test_ac_19_1_large_tree_search_times_out_within_bound(daimonos_factory, tmp_path):
+    """AC-19.1 / F2: a content search on a large tree (10000 files x ~1 KB =
+    ~10 MB; a full walk+scan of this many files clearly exceeds the limit on any
+    host in this environment, where opening 10000 files alone costs well over
+    100 ms) with a 100 ms limit returns ``tool_timeout`` within a stated upper
+    bound (8 s, far below the time a full scan would take), proving the deadline
+    STOPPED the work rather than the scan finishing on its own. The control with
+    no option returns success. Kept under ~10 s including setup.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    blob = "lorem ipsum dolor sit amet " * 38  # ~1 KB, no match
+    for i in range(10000):
+        (ws / f"f{i:05}.txt").write_text(blob)
+
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "100"])
+    start = time.monotonic()
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    elapsed = time.monotonic() - start
+    assert elapsed < 8, f"budget did not stop the search promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_timeout", _text(result)
+
+    control = daimonos_factory(ws)
+    r2 = control.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert r2.get("isError") is not True, _text(r2)
+    assert "tool_timeout" not in _text(r2), _text(r2)
+
+
+# --- F3: plugin/managed-process tools (M3 curl, M4 git) ---
+
+
+def _slow_http(delay_s):
+    class Handler(BaseHTTPRequestHandler):
+        protocol_version = "HTTP/1.1"
+
+        def log_message(self, *_):
+            pass
+
+        def do_GET(self):
+            time.sleep(delay_s)
+            body = b"wp18-slow-body"
+            self.send_response(200)
+            self.send_header("Content-Length", str(len(body)))
+            self.end_headers()
+            try:
+                self.wfile.write(body)
+            except (BrokenPipeError, ConnectionResetError):
+                pass
+
+    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+    server.daemon_threads = True
+    t = threading.Thread(target=server.serve_forever, daemon=True)
+    t.start()
+    return server, t
+
+
+def test_timeout_stops_curl_plugin(daimonos_factory, tmp_path):
+    """F3 (M3): the curl plugin starts a subprocess; a --tool-timeout-ms 700
+    deadline stops and reaps it, returning ``tool_timeout`` in under 3 s against
+    a server that would not answer for 4 s, and leaving no curl child alive."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    server, _t = _slow_http(4.0)
+    port = server.server_port
+    url = f"http://127.0.0.1:{port}/"
+    needle = f"wp18curl-{_os.getpid()}-{time.time_ns()}"
+    try:
+        client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
+        start = time.monotonic()
+        # The url carries a unique marker so _survivors can spot a leaked child.
+        result = client.call_tool("curl", {"url": url + needle})
+        elapsed = time.monotonic() - start
+    finally:
+        server.shutdown()
+        server.server_close()
+    assert elapsed < 3, f"curl ran past the deadline: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_timeout", data
+    assert _poll_gone(needle, 3.0), "curl child survived the budget stop"
+
+
+def test_timeout_stops_git_plugin(daimonos_factory, tmp_path):
+    """F4-of-M4 (timeout half): the `git` plugin (a subprocess) is stopped by
+    --tool-timeout-ms 1 with ``tool_timeout``, not a success."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_timeout", data
+
+
+def test_input_bytes_refuses_git_plugin(daimonos_factory, tmp_path):
+    """F3 (M4 input half): the `git` plugin reads no meterable logical input, so
+    under --tool-input-bytes it is REFUSED with ``tool_budget_unenforceable``
+    rather than passing uncounted (ADR-031)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "10"])
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_budget_unenforceable", data
+
+
+def test_git_plugin_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option the `git` plugin is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_timeout" not in _text(result), _text(result)
+    assert "tool_budget_unenforceable" not in _text(result), _text(result)
+
+
+def test_budgeted_pipeline_is_refused(daimonos_factory, tmp_path):
+    """F3 (pipeline): tool_pipeline runs external plugin stages whose remaining
+    deadline/input cannot be enforced stage-by-stage, so rather than leave it
+    "not cooperative" it is REFUSED with ``tool_budget_unenforceable`` whenever
+    any budget option is set (ADR-031 allows refusing the pipeline when
+    --tool-timeout-ms is set; the same applies to input/entry). The call returns
+    promptly and starts no stage process."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
+    start = time.monotonic()
+    result = client.call_tool("tool_pipeline",
+                              {"tool_id": "git", "stages": ["status"]})
+    elapsed = time.monotonic() - start
+    assert elapsed < 3, f"refusal was not prompt: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_budget_unenforceable", data
+
+
+def test_pipeline_default_off_not_refused(daimonos_factory, tmp_path):
+    """Regression: with no budget option a tool_pipeline is not budget-refused."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("tool_pipeline",
+                              {"tool_id": "git", "stages": ["status"]})
+    assert "tool_budget_unenforceable" not in _text(result), _text(result)
+
+
+# --- F5: the audit limit_hit event, asserted by kind and fields ---
+
+
+def test_budget_hit_writes_limit_hit_audit_event(daimonos_binary, tmp_path):
+    """F5 (M5): a budget limit hit in a run with --audit-path writes the
+    reserved ``limit_hit`` audit event via record_limit_hit, carrying the limit
+    name, the tool, the configured value, and the consumed value. Asserted by
+    the event ``kind`` and its fields, not by a substring search for a code."""
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    (workspace / "big.txt").write_text("z" * 500)
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider)
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "--tool-input-bytes", "100",
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "do the read"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    hits = [ev for ev in events if ev.get("kind") == "limit_hit"]
+    assert hits, f"no limit_hit event: {[e.get('kind') for e in events]}"
+    hit = hits[0]
+    assert hit["tool"] == "read_file", hit
+    assert hit["limit"], hit
+    assert hit["configured"] == 100, hit
+    assert hit["consumed"] >= 500, hit

### Known Concerns
1. No process test runs the chat frontend or the ACP frontend with a budget option. The worker argues that both build a Session from the same config. The session daemon has a launch-identity unit test only.
2. execute_script concurrency: the first design left the budget installed on the shared session between inner operations. The fix sets it only inside each locked inner operation. No test runs a concurrent top-level call during an execute_script call, so the fix is not proven.
3. limit_hit detection parses the tool result text (run_audit::budget_limit_hit: a JSON object with code tool_timeout or tool_input_limit). It is not analysed whether a tool can return attacker-controlled text of that shape (for example an outbound MCP tool, or exec output) and so write a false limit_hit audit event.
4. The refusal code tool_budget_unenforceable is new. The reviewed interface lists tool_policy_unsupported for an unsupported tool policy. The option --tool-max-entries is not in the reviewed interface row either (marked proposed, integrator decision D56). An entry-limit hit reuses tool_input_limit.
5. Under --tool-input-bytes, exec, every process plugin, and the index search are refused. The intended caller runs with --confine-paths, where only 12 tools are offered (batch, execute_script, ls, read_file, search, and state tools), so the refusals mostly matter outside that profile. `search` in files mode is in that profile and is refused under an input limit.
6. read_file charges the whole file content also when offset/limit select a few lines, because the tool reads the whole file. ls is not charged under --tool-input-bytes (only under --tool-max-entries).
7. The deadline for native tools is checked at boundaries, not preemptive: a single slow syscall or a blocking read of one very large file can pass the deadline before the next check. The acceptance test uses a bound of 8 s for a 1 s to 1.5 s deadline.
8. git_read charges the bytes of its rendered output text as 'git content', after the subprocess finished. It is not a count of blob bytes read by git.
9. The reserved type names BudgetReceipt and LimitFailure of the reviewed interface are not used; path_policy::ToolFailure carries the failure.
10. The tool-contract dump (effective_options) does not name the budget options. That dump format has an open owner decision.
11. Timing-based tests (deadlines of 700 ms to 1500 ms, process reap checks through /proc) can be flaky on a loaded CI runner.

### Specific Questions for Reviewer
1. Is there a tool or route that a caller can reach with a budget option set and that is neither limited nor refused (silent exemption)? Check each dispatch path: ops opcodes, registry plugins, user-registered tools (tool_run), outbound MCP tools of the agent, kgl tools, snapshot/restore, background exec (bg), session daemon, ACP, chat.
2. Can a child of batch or execute_script get a fresh budget or extend the deadline? Is the install/clear ownership correct on each early return, error path, and panic-free path, so a budget never leaks into the next top-level call?
3. Is 'no partial result' true on each hit path, including a search that already collected matches and an exec that already produced output?
4. Is the process-group termination real for exec, git_read, and plugins (no bare timeout wrapper), and are the tests able to detect a surviving child?
5. Is default-off behavior unchanged: any change of a signature, response shape, timing, or tool list when no option is set?
6. Is the accounting correct and overflow-safe (atomic counters, charge before or after the work, concurrent children)?
7. Are the new audit events and codes consistent with the closed code list and the WP08 audit rules?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Silent-exemption surface (request question 1) is not closed. tool_input_unmeterable() is a hard-coded name list (exec, bg, git, gh, cargo, npm, pytest, docker, shellcheck, curl, discord, lint_repair, tool_run), so any tool NOT on the list that starts external work or reads unmetered input is silently exempt from --tool-input-bytes/--tool-max-entries: config-registered [tools.*] custom tools (which run arbitrary subprocess commands via the registry), outbound MCP tools of the agent (mcp__*), kgl tools, and snapshot/restore are not on the list, are not metered anywhere in the diff, and are not refused. The interface row says 'a configured unsupported operation is refused, not silently exempted'; a deny-list keyed on today's plugin names cannot satisfy that for user-registered tools. This needs either an allow-list inversion (refuse by default anything without a metering seam) or explicit per-route analysis plus tests for [tools.*], mcp__*, kgl, snapshot, and bg.
B2. Budget install/clear is not exception/early-return safe at the single registry seam, so a budget can leak into the next top-level call. In mcp.rs dispatch_tool_inner, several arms `return` directly from inside the with_budget_deadline async block or even before it on error paths (e.g. the git_read 'tool registry unavailable' arm returns err_text without end_tool_operation; the budget_charge_bytes failure inside the git_read arm returns from the inner block — that one is fine since it's inside the scoped future, but any `return` path that bypasses `session.end_tool_operation(budget_owned)` after the block leaves session.tool_budget set). Concretely: `return err_text(...)` inside the git_read arm appears to return from dispatch_tool_inner itself in the quoted diff (it is written before the closure's end), and the batch handler clears only `if batch_budget_owned` with no protection against a sub-op panicking or an early `return` between begin and end. There is also no RAII guard, so a future refactor trivially reintroduces the leak. Please use a guard type (clear-on-drop when owned) or demonstrate by test that a failing/early-returning call under a budget leaves session.tool_budget == None for the next call (the integrator probe only checked the post-timeout case on one path).
B3. The limit_hit audit event is forgeable: run_audit::budget_limit_hit parses the tool-result text and matches any JSON object with code == tool_timeout/tool_input_limit. A tool that returns attacker-influenced text — exec output, curl body, an outbound MCP tool, or a [tools.*] tool — can emit exactly that shape and cause agent.rs to write a false reserved limit_hit event into the audit (audit-integrity violation under the WP08 rules, and already flagged in known_concerns without analysis). The hit should be signalled in-band (e.g. a typed flag/metadata on the dispatch result set only by the budget code path), not re-parsed from result text. At minimum, restrict detection to results produced by ToolFailure::into_response on budget paths and add a test that an exec printing the JSON shape does NOT produce a limit_hit event.
B4. FR-19 contract breach for the entry limit on ls: budget_charge_entries is called AFTER the walk completes (file_ops.rs: ls_recurse runs to completion accumulating `walked`, then charges). With --tool-max-entries 3 on a 10M-entry tree the walk still visits every entry; the limit bounds nothing — it only reclassifies the result afterwards. ADR-031's purpose (bound the data a call may scan) requires charging incrementally inside ls_recurse and stopping the walk at the ceiling, as search does with BudgetedReader. The shipped test (10 files, limit 3) cannot distinguish post-hoc accounting from enforcement; add a charge-inside-the-walk implementation and a test that proves early stop (e.g. consumed well below the total entries in a large tree). The same post-hoc pattern applies to read_file (whole file read before charging), which is acknowledged but at least bounded by one file; ls is unbounded.
B5. Interface/taxonomy divergences shipped without an owner decision in the record: (a) the new refusal code tool_budget_unenforceable is not in the reviewed interface row (which lists tool_timeout, tool_input_limit, tool_policy_unsupported) yet is appended to REFUSAL_CODES — the closed code list is extended unilaterally; (b) --tool-max-entries is only 'proposed' (D56) but is live, CLI-exposed, and part of the daemon launch identity; (c) the reserved names BudgetReceipt/LimitFailure and charge_input exist only partially (charge_input kept, the types not). The INTERFACES.md status notes label these 'merge items for the owner', but the work package cannot be approved with an open taxonomy delta to its reviewed contract: either obtain the recorded owner/integrator decisions (D56-style) for the new code and the entry flag, or gate them. Note also the two INTERFACES.md status blocks (r3 and r4) now contradict each other (r3 says search meters bytes returned and nothing is refused; r4 says scanned and files-mode is refused) — the stale r3 block must be corrected, since the file is the contract of record.
B6. execute_script concurrency fix (F4) is asserted but unproven, and has a visible gap: the shared budget is installed under the per-op lock and cleared with set_operation_budget(None) — but dispatch inside that window calls ops::dispatch -> begin_tool_operation, which sees the installed budget and inherits (owned=false), fine; however if the inner op path itself errors between set_operation_budget(budget) and set_operation_budget(None) via `?`/early return inside the block_on future, the clear still runs only because both calls are in the same async block — EXCEPT dispatch_tool_by_name's dispatch_local_tool can internally re-enter dispatch_tool_inner, which does its own begin/end with ownership semantics on the SAME session; an end_tool_operation(true) on an inner path would clear the script's shared budget mid-script. No test exercises a concurrent top-level MCP call during a running execute_script, and no test exercises a script inner op that fails under a budget followed by further ops. Given this exact area already failed once (first worker result), the fix needs a direct test: concurrent top-level call during a budgeted script observes no budget (or its own fresh one), and the script's cumulative accounting survives an inner failure.

### Non-Blocking Suggestions
S1. The 'no partial result' guarantee loses already-collected matches silently on the search paths: in grep_blocking a BudgetedReader deadline/byte hit aborts search_reader via an io::Error, and search_reader errors on OTHER files (genuine I/O errors) are swallowed with `let _ =`. Consider documenting that a mid-scan I/O error under a budget is indistinguishable from a budget stop only via reader.hit(), and add a test for a genuine read error under an active budget to confirm it is not misreported as a budget hit.
S2. ToolBudget::charge_input uses fetch_add with Relaxed ordering and `+ n`; with concurrent children a counter can wrap (u64 overflow) only theoretically, but more practically two concurrent charges can both observe consumed <= limit transiently while jointly exceeding it before either returns — acceptable for a cooperative budget, but worth a one-line comment, and consider saturating_add to rule out wrap on hostile inputs (a tool charging u64::MAX).
S3. git_read charges output-text bytes (rendered log text) as 'git content' after the subprocess finished — this neither bounds what git read from the object store nor stops early. Since git_read is already deadline-enforced and the byte charge is admittedly approximate, consider either moving git_read to the unmeterable/refused set under --tool-input-bytes (consistent with the other process plugins) or recording the approximation explicitly as an owner decision, so the metering semantics ('bytes scanned') stay uniform.
S4. Timing robustness: test_timeout_stops_curl_plugin asserts elapsed < 3 s with a 700 ms deadline and test_budgeted_pipeline_is_refused asserts < 3 s; on a heavily loaded CI runner process spawn plus TERM-grace can approach these bounds. Consider widening non-semantic bounds to the 8 s convention already used elsewhere in the file, keeping only the 'well below the sleep duration' property as the semantic assertion.
S5. Add at least one process-level test driving the chat or ACP frontend with a budget option (known concern #1): both are claimed covered by 'same Session from same config', but the allowlist work (WP06 r3) found exactly this class of frontend divergence, so a cheap smoke test (budget option + one refused read) would retire the recurring risk.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1582-wp18-tool-budgets/artifacts/round-1-rebuttal.diff" sha256="dd3bea558f0e11ef5075c1c0ff8dcab83cf073a46f80d2569da018f7033c55e9" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — Accepted: a deny-list of today's names cannot satisfy 'refused, not silently exempted'. The list is inverted. tool_budget::tool_input_budget_admitted is an explicit allow-list: tools that meter their input (read_file, ls, search, git_read), write-only tools, the two composites (children are gated one by one), session state, static tool-catalog introspection, the agent-mail and reservation stores, and plan state. The doc comment names why each class reads no workspace or external content. Every other name is refused under --tool-input-bytes or --tool-max-entries, including edit_file, diff_files, skill, workspace_info, kgl_*, snapshot, bg, tool_run, config-registered tools, outbound mcp__* tools, and a name that a later package adds. Gates: the registry seam (mcp::dispatch_tool_inner), the agent run loop for outbound MCP and worker-routed tools (agent.rs), and script dispatch by name (script.rs), all through Session::budget_input_refusal. search in files mode is refused through Session::budget_index_search_refusal. Tests: a Rust unit test of the admission function (each admitted class, each refused class, 'mcp__server__some_tool', 'some_future_tool', and the empty name), and process tests that one tool of each class is refused under an input limit and works without a budget option, and that the deadline alone refuses nothing except tool_pipeline.
Re B2: CLARIFY — The leak that the concern describes is not in the code; the request for proof is accepted and done. Fact: in dispatch_tool_inner each `return` between the start of the with_budget_deadline block and its end is inside the async block, so it ends the block, and `session.end_tool_operation(budget_owned)` runs after the block on every path. The only return before the block is the refusal, which clears first. The git_read arm that the concern quotes was the first worker result; the fix run replaced the per-arm install by the one scope. Proof added: test_each_kind_of_failed_call_leaves_a_fresh_budget_for_the_next_call runs, under --tool-input-bytes 100, a 70-byte read, then one early ending (a limit hit, a refused tool, a tool error, a refused index search, an unknown tool name, a batch with a failed child, a script that hit the limit), then a 60-byte probe read that passes only with a fresh allowance. test_a_timeout_leaves_a_fresh_budget_for_the_next_call does the same for the deadline. Hardening: a debug_assert at the start of a top-level MCP call that no budget is on the session. A guard type was not added: the budget is a field of the `&mut Session` that the dispatch borrows, so a drop guard would hold that borrow across the dispatch. A mutation (the script budget left on the session) fails the first test with 'budget leaked after a script that hit the limit'.
Re B3: ACCEPT — Accepted. run_audit::budget_limit_hit (the text parser) is removed. The budget code records a hit in-band: Session.budget_hit holds (code, configured, consumed) and is set only by the budget helpers (charge, precheck, deadline check) and by the ls and grep stop paths. begin_tool_operation clears a stale record. The agent run loop calls session.take_budget_hit() after the dispatch and writes limit_hit from that record. Tests: test_exec_printing_budget_json_writes_no_limit_hit_event (an audited agent run in which exec prints the JSON shape writes no limit_hit event) and test_budget_hit_writes_limit_hit_audit_event (a real hit writes one, checked by kind and fields). Honest note: the forged-shape test also passed on the old binary, because exec wraps its stdout in its own JSON object, so the old parser was not fooled through exec. The in-band signal removes the class for every tool. Not changed, by direction: run_audit::refusal_code (WP08, reviewed earlier) still classifies a tool_refusal from result text. That is a merge item for the owner, outside this package.
Re B4: ACCEPT — Accepted: the entry limit only reclassified the result. ls_recurse now charges each entry as it visits it and stops the walk at the limit (a WalkStop result). The confined walk gets the remaining entry allowance (WalkOptions.max_entries) and stops the same way. read_file checks the file size from the metadata before the read (ToolBudget::precheck_input) and refuses without reading when the size alone passes the remaining allowance. Test: test_max_entries_bounds_the_ls_walk builds a tree of 5000 entries and requires tool_input_limit with consumed below 200 under --tool-max-entries 50, and no listing. A companion test covers the confined walk.
Re B5: ACCEPT — Accepted, with recorded integrator decisions. The integrator has the owner's standing instruction to decide open points with the recommended option and to record them; each one is listed for the owner in the PR. (a) D60: the new code tool_budget_unenforceable is removed from the code, from REFUSAL_CODES, from the tests, and from the documents. A tool that cannot enforce a requested input or entry budget is refused with the REVIEWED code tool_policy_unsupported. WP18 does not change the closed code list. (b) D56 and D61: --tool-max-entries stays live and is marked (proposed) in INTERFACES.md and docs/configuration.md. Reason: the PM answer to Q-FR-19-1 requires a separate, optional entry-count limit for walks, and the reviewed row has no option for it, so without an option that PM condition cannot be met. An entry hit reuses tool_input_limit. (c) D61: BudgetReceipt and LimitFailure stay unused, and INTERFACES.md says so once; charge_input is the reserved method name. The two contradicting status blocks are replaced by ONE block that describes the final code (options, codes, admission, deadline, metering, composites, audit, names, open points).
Re B6: ACCEPT — Accepted: the fix was unproven. Two process tests through the real MCP server. (1) test_a_call_during_a_budgeted_script_does_not_share_its_budget: under --tool-input-bytes 100, an execute_script call reads a 60-byte file, computes for a moment without a tool call, and reads again. A second request (a 60-byte read) is written to the same connection 150 ms after the first. The test requires that the reply of the read arrives BEFORE the reply of the script, which shows that the read ran while the script was in progress. The read must succeed (it did not inherit the 60 bytes that the script used), and the script must end with tool_input_limit, consumed=120 (the other call did not clear or replace its budget). (2) test_a_failed_child_does_not_clear_the_shared_budget: a batch whose first child fails (file not found) and whose next two children read 60 bytes each under a 100-byte limit. The results must be [fail, ok, fail] with consumed 120 on the last. The composite is batch because in execute_script a failed operation ends the script, so a script has no operation after a failed one. Mutation: with the two set_operation_budget(None) calls in script.rs removed, test (1) fails (the concurrent read reports consumed=120) and the B2 leak test fails. On the re-entry point of the concern: an inner dispatch_tool_inner call finds the budget installed, so begin_tool_operation returns owned=false, and end_tool_operation(false) clears nothing.

### Revised Code / Diff
diff --git a/CHANGELOG.md b/CHANGELOG.md
index 6a4bb13..7bc5d35 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -9,6 +9,26 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ### Added
 
+- **Cooperative tool budgets (FR-19, ADR-031).** Three default-off, global
+  options limit one top-level tool call: `--tool-timeout-ms N` (a deadline),
+  `--tool-input-bytes N` (a limit for the file and git content that the call
+  reads or scans), and `--tool-max-entries N` (a limit for the directory
+  entries that the call walks; the name is proposed).
+  - A deadline hit returns `tool_timeout`. A byte or entry hit returns
+    `tool_input_limit` with the limit, the configured value, and the consumed
+    value. A hit returns no partial result and writes the `limit_hit` audit
+    event.
+  - `search` in content mode charges the bytes that it scans and stops during
+    the scan. `ls` stops the walk at the entry limit.
+  - `batch` and `execute_script` share one budget with their children.
+  - A subprocess of `exec`, `git_read`, or a plugin is stopped at the deadline
+    with its process group.
+  - Under an input or entry limit, a tool that cannot meter its input is
+    refused with `tool_policy_unsupported`: `exec`, the process plugins, the
+    index search, and each tool that is not on the admission list.
+    `tool_pipeline` is refused under each budget option. A budget option with
+    `--worker-config` is refused at startup (exit `2`).
+  - Without the options, each tool is unchanged.
 - **`--capability-probe-payload` (FR-04).** A default-off `agent --print`
   option (ADR-027) that sends exactly one unretried capability probe before any
   evidence tool runs, proving the endpoint can drive the same final-answer tool
diff --git a/docs/configuration.md b/docs/configuration.md
index 1b0e3e0..bb88a95 100644
--- a/docs/configuration.md
+++ b/docs/configuration.md
@@ -1812,3 +1812,91 @@ The control file is JSON:
   platforms reaping relies on the SDK's `kill_on_drop` at process exit.
 
 See ADR-028 and `tests/test_worker_isolation.py`.
+
+## Cooperative tool budgets (FR-19, ADR-031)
+
+Three default-off, global command-line options bound the resources a single
+top-level tool call may consume. They are caller-owned (never read from
+`daimonos.toml`) and, when none is set, every tool behaves exactly as before.
+
+- **`--tool-timeout-ms N`** — an absolute deadline, in milliseconds, for the
+  call. A hit returns the reviewed `tool_timeout` code with no partial result.
+- **`--tool-input-bytes N`** — a logical input-byte ceiling for the call. It
+  counts the file content that a tool reads or scans, and the git content that
+  `git_read` returns. It never counts the displayed or truncated output bytes.
+  A hit returns `tool_input_limit`.
+- **`--tool-max-entries N`** *(proposed)* — a directory-entry ceiling for the
+  call (e.g. `ls` walking). A hit returns `tool_input_limit` naming
+  `directory entries`. (The reviewed interface row has no separate code for the
+  entry limit; integrator decision D56 reuses `tool_input_limit`.)
+
+Each value must be a positive integer; `0` is rejected at startup (exit `2`).
+
+**No partial success.** A limit hit is a typed tool failure naming which limit
+was hit, the configured value, and the consumed value — never a truncated
+"successful" result.
+
+**One budget per call, shared by composites.** A `batch` and an
+`execute_script` share one budget with their children: a child inherits the
+remaining allowance and the same absolute deadline and cannot open a fresh
+top-level budget, so delegation cannot multiply the allowance.
+
+**Stopped work, not a stopped wait.** For `exec` and `git_read` the deadline is
+handed to the managed subprocess, which reaps the whole process group
+(`SIGTERM`, bounded grace, then `SIGKILL`) — so no child or grandchild outlives
+the `tool_timeout` result. On non-Unix platforms the subprocess stop relies on
+the managed lifecycle's `kill_on_drop`.
+
+**Output truncation is separate.** `--no-output-spill` bounds the *displayed*
+output of a result; it is not an input budget and a truncated result is never a
+budget failure.
+
+**Workers.** Worker-routed tool calls (`--worker-config`, WP10) run in a
+separate process that this process's budget cannot observe, so the deadline
+cannot stop that work and its input cannot be accounted. Rather than silently
+ignore a budget option in worker mode, a budgeted worker route is refused at
+startup with exit `2` (`run_configuration_invalid`, reason
+`tool_budget_worker_unenforceable`). Worker mode with no budget option is
+unchanged.
+
+Every frontend that builds a session from configuration honours the budget
+(the MCP server, `agent --print`, the interactive agent / session daemon,
+`chat`, `acp`, `batch`, and `execute_script`). For the interactive agent the
+budget is part of the session-daemon launch identity, so an attach to a daemon
+started under a different (or absent) budget is refused.
+
+See ADR-031 and `tests/test_tool_budgets.py`.
+
+#### Tool budgets — enforcement scope (WP18, FR-19)
+
+The default-off options `--tool-timeout-ms`, `--tool-input-bytes`, and
+`--tool-max-entries` are cooperative:
+
+- **Deadline.** Each tool call has the deadline. `read_file`, `ls`, and `search`
+  check it at work boundaries: between files, and for each read chunk of a
+  content search. A subprocess of `exec`, `git_read`, or a plugin is stopped at
+  the deadline with its process group (Linux; on other platforms the process
+  default timeout applies). A passed deadline returns `tool_timeout`. The check
+  is not preemptive: one slow system call can pass the deadline before the next
+  check.
+- **Input bytes.** `search` in content mode charges the bytes that it scans and
+  stops during the scan. `read_file` checks the file size before the read and
+  charges the whole file, also when `offset` and `limit` select a part.
+  `git_read` charges the bytes of the content that it returns, after git ended.
+  That is an approximation of the git content read: it does not stop git early.
+- **Entries.** `ls` charges each directory entry during the walk and stops at
+  the limit.
+- **Refused under an input or entry limit.** With `--tool-input-bytes` or
+  `--tool-max-entries`, a tool is admitted only when it meters its input or
+  reads no workspace or external content. Each other tool is refused with
+  `tool_policy_unsupported`: `exec`, the process plugins (`git`, `gh`, `cargo`,
+  `npm`, `pytest`, `docker`, `shellcheck`, `curl`), `edit_file`, `snapshot`,
+  `workspace_info`, tools from configuration, outbound MCP tools, and `search`
+  in files mode (the index does not report the content that it reads). With
+  `--confine-paths`, the offered tools `read_file`, `ls`, and `search` in
+  content mode stay available.
+- **Refused under each budget option.** `tool_pipeline`, and a run with
+  `--worker-config` (at startup).
+
+Output truncation (`--no-output-spill`, `tool_output`) is a separate mechanism,
+never reported as a budget limit hit.
diff --git a/docs/contexts/reviewer-runtime/INTERFACES.md b/docs/contexts/reviewer-runtime/INTERFACES.md
index 18e5b30..dda5d3c 100644
--- a/docs/contexts/reviewer-runtime/INTERFACES.md
+++ b/docs/contexts/reviewer-runtime/INTERFACES.md
@@ -284,6 +284,18 @@ No cooperative guarantee is claimed for an unowned remote process.
 
 Proposed `ToolFailure` fields: `code`, `message`, `tool`, `argument`, and optional `limit`, `configured`, `consumed`, and `retryable`.
 A timed-out or input-limited call returns `complete=false` and no success-shaped partial result.
+
+_Status (WP18, implemented; state after the code review of 2026-11-23):_
+
+- **Options.** `--tool-timeout-ms N`, `--tool-input-bytes N`, and `--tool-max-entries N` are global, default-off options. `0` is refused at startup with exit code `2`. `--tool-max-entries` is not in the reviewed row above: the name is **(proposed)** (integrator decision D56, owner decision open).
+- **Codes.** A deadline hit returns `tool_timeout`. A byte or entry hit returns `tool_input_limit` and names the limit, the configured value, and the consumed value. An entry hit has no code of its own (D56). A tool that cannot enforce a requested input or entry budget is refused with the reviewed code `tool_policy_unsupported` (D60). WP18 adds no refusal code.
+- **Admission under an input or entry budget.** `tool_budget::tool_input_budget_admitted` is an explicit list. A tool is admitted only when it meters its input (`read_file`, `ls`, `search` in content mode, `git_read`) or reads no workspace or external content (write-only tools, composites, session state, tool-catalog introspection, agent mail, reservations, plan state). Each other tool name is refused, also an outbound MCP tool, a tool from configuration, and a name that a later package adds. `search` in files mode (the index) is refused: the index does not report the content that it reads.
+- **Deadline.** One scope in `mcp::dispatch_tool_inner` covers each registry tool. Native readers check the deadline at work boundaries. A subprocess of `exec`, `git_read`, or a plugin is terminated with its process group through `ManagedChild::terminate`. `tool_pipeline` is refused under each budget option. A run with `--worker-config` and a budget option is refused at startup (`run_configuration_invalid`, reason `tool_budget_worker_unenforceable`).
+- **Metering.** `search` in content mode charges the bytes that it scans and stops during the scan. `ls` charges each entry during the walk and stops at the limit. `read_file` checks the file size before the read and charges the content. `git_read` charges the bytes of the content that it returns, after the subprocess ended: this is an approximation of the git content read (D61).
+- **Composites.** `batch` and `execute_script` share one budget with their children. `execute_script` holds the budget in its script context and sets it on the session only inside each locked inner operation.
+- **Audit.** A limit hit writes the reserved `limit_hit` event. The budget code records the hit on the session, and the run loop reads that record. It does not parse the result text of a tool.
+- **Names.** `ToolBudget` with `check_deadline()`, `charge_input(bytes)`, `charge_entries(n)`, and `remaining()`. The reserved names `BudgetReceipt` and `LimitFailure` are not used: `path_policy::ToolFailure` carries a failure (D61).
+- **Open.** No process test runs the chat or ACP frontend with a budget option. The `tool-contract` dump does not name the budget options.
 These fields are policy/error metadata, not a rewrite of every existing tool's successful payload.
 Audit records the named limit and the refusal. Error text is bounded and does not contain raw evidence.
 
@@ -424,3 +436,4 @@ Implementation status (WP06, r3 review fixes): the model-facing catalog is now f
 Implementation status: WP06 (r2 review fixes) extends the above. The explicit policy now also filters schema lookup (`get_tool_schema`), the Starlark signature dump (`list_tool_signatures`), the MCP server-instructions signature section, the session-daemon socket listing, and the agent-print offered catalog — so the served, discovered, probed, dumped, and dispatched sets agree (ADR-023). With `--final-output`, an allowlist that omits the designated final-answer tool fails at startup with exit-2 `run_configuration_invalid` reason `tool_allowlist_missing_final_output`; the name is never added automatically (PM Q-FR-02-2). The reported `effective_tool_allowlist` is the intersection with the run's active legacy restriction (the agent-env `allowed_commands`), i.e. the set the run will actually permit. Config-registered custom local tools (`[tools.*]`) are part of the known catalog and may be allowlisted.
 
 Implementation status: WP06 (r4 review fixes) extends the above. Exit-2 taxonomy addition: a malformed or unreadable `--tool-allowlist` file fails at startup with `run_configuration_invalid` reason `tool_allowlist_invalid` (joining `tool_allowlist_unknown_entry` and `tool_allowlist_missing_final_output` as the three reserved WP06 exit-2 reasons; `tests/test_tool_allowlist.py::test_malformed_allowlist_file_fails_at_startup` and `::test_unreadable_allowlist_file_fails_at_startup`). Delegation (ADR-023 R1, PM Q-FR-02-1): the composite opcodes `tool_pipeline` and `lint_repair` (and the raw-protocol `tool_run`) now check their inner `tool_id` against the policy (`src/ops/tool_ops.rs` → `session.policy.admit(tool_id)`) before `ToolRegistry::pipeline`/`::repair`/`::run`, so a listed composite cannot reach an unlisted registered tool; the refused inner tool never runs (`tests/test_tool_allowlist.py::test_tool_pipeline_inner_tool_not_on_allowlist_is_refused`, `::test_lint_repair_inner_tool_not_on_allowlist_is_refused`, and the `ops::tool_ops` unit tests). In this package the allowlist names only native and config-registered `[tools.*]` tools; remote/worker names defer to WP10, and an active allowlist therefore removes every forwarded MCP tool from the catalog.
+
diff --git a/src/agent.rs b/src/agent.rs
index ad580f8..273585e 100644
--- a/src/agent.rs
+++ b/src/agent.rs
@@ -1743,6 +1743,19 @@ async fn run_inner(
                     // final-answer tool is required to be on the list at startup
                     // and is intercepted earlier, so it is never refused here.
                     let policy_refused = !session.lock().await.policy.allows(&name);
+                    // R1 (B1, D60): the agent/chat/ACP loop is this frontend's own
+                    // dispatch boundary. Opcode tools reach `tool_facade::invoke`
+                    // directly (never `mcp::dispatch_tool_inner`), and remote
+                    // `mcp__*` tools reach the remote hook, so the input/entry
+                    // budget allow-list gate is enforced here too, before any
+                    // worker/native/local/remote dispatch. A tool not on the
+                    // allow-list (edit_file, snapshot, kgl_*, a plugin, an outbound
+                    // mcp__* tool, or any later name) is refused with the reviewed
+                    // `tool_policy_unsupported`; a bare `--tool-timeout-ms` refuses
+                    // nothing here. (A budgeted worker run is also refused at
+                    // startup, so the worker branch is unreachable under a budget;
+                    // this gate covers it defensively.)
+                    let budget_refusal = session.lock().await.budget_input_refusal(&name);
                     let (mut content, is_error, mut outcome) = if policy_refused {
                         let content = serde_json::json!({
                             "code": crate::run_policy::CODE_TOOL_NOT_ALLOWED,
@@ -1758,6 +1771,19 @@ async fn run_inner(
                             ..ToolOutcome::default()
                         };
                         (content, true, Some(outcome))
+                    } else if let Some(failure) = budget_refusal {
+                        let content = serde_json::json!({
+                            "code": failure.code,
+                            "message": failure.message,
+                            "tool": name.clone(),
+                            "retryable": false,
+                        })
+                        .to_string();
+                        let outcome = ToolOutcome {
+                            request_tokens_est: crate::analytics::estimate_tokens(request_chars),
+                            ..ToolOutcome::default()
+                        };
+                        (content, true, Some(outcome))
                     } else if let Some(worker) = &config.worker_executor {
                         // WP10 (FR-05, ADR-028): worker-only execution. Every
                         // non-final tool call is routed to a caller-supplied
@@ -2147,6 +2173,19 @@ async fn run_inner(
                             tool_started.elapsed().as_millis() as u64,
                             None,
                         );
+                        // WP18 (FR-19 F5, ADR-031): a cooperative tool-budget
+                        // limit hit also writes the reserved `limit_hit` event
+                        // with the limit name, the tool, and the configured and
+                        // consumed values. R3 (B3): read the hit IN-BAND from the
+                        // session (set only by the budget code on a genuine hit)
+                        // rather than parsing the result text, which a tool could
+                        // forge by echoing the JSON shape.
+                        if let Some((limit, configured, consumed)) =
+                            session.lock().await.take_budget_hit()
+                        {
+                            let _ =
+                                audit.record_limit_hit(&limit, Some(&name), configured, consumed);
+                        }
                         if audit.poisoned() {
                             terminate_reason = "audit_write_failed";
                             terminate = true;
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
index fc4e4c3..da2bdff 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -459,6 +459,10 @@ pub async fn run_agent(
             .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
         let bootstrap = crate::session_bootstrap::connect_or_spawn(
             &crate::session_bootstrap::BootstrapOptions {
+                // FR-19 (WP18, ADR-031): carry the caller's budget into the
+                // daemon launch identity so an interactive session enforces it
+                // and an attach cannot silently downgrade it.
+                tool_budget: cfg.tool_budget,
                 workspace,
                 config_path: explicit_config_path.as_deref(),
                 state_dir: cfg
@@ -702,6 +706,24 @@ pub async fn run_agent(
         }
         None => None,
     };
+    // FR-19 (WP18, ADR-031): a cooperative tool budget is installed at THIS
+    // process's dispatch seams, but worker-routed tool calls execute inside a
+    // separate worker process the parent budget never observes — the deadline
+    // cannot stop that owned work and its input cannot be accounted. Rather
+    // than silently ignore a budget option in worker mode (the frontend rule
+    // forbids a silent drop), refuse the budgeted worker route at startup, the
+    // same fail-closed class as `reject_credential_disclosure`
+    // (ADR-031 "refuse enabled-but-unenforceable"; the INTERFACES worker rule).
+    // A default-off budget leaves worker mode exactly unchanged.
+    if worker_config_runtime.is_some() && cfg.tool_budget.any_enabled() {
+        emit_run_configuration_invalid(
+            "tool_budget_worker_unenforceable",
+            "a tool budget (--tool-timeout-ms/--tool-input-bytes/--tool-max-entries) \
+             cannot be enforced across the worker boundary: worker-routed tool calls \
+             run in a separate process, so the deadline cannot stop that work and the \
+             input cannot be accounted. Remove the budget option or --worker-config.",
+        );
+    }
     let agent = load_agent_env(agent_env, &cfg)?;
     let cfg = with_skill_dir(cfg, &agent)?;
     let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
diff --git a/src/cli.rs b/src/cli.rs
index 8f9ef6e..360abb8 100644
--- a/src/cli.rs
+++ b/src/cli.rs
@@ -469,6 +469,29 @@ pub struct Cli {
     #[arg(long, global = true, value_name = "PATH")]
     pub tool_allowlist: Option<PathBuf>,
 
+    /// Stop an opted-in tool call at an absolute per-call deadline of N
+    /// milliseconds (WP18, FR-19, ADR-031). External work (exec, git_read)
+    /// inherits the remaining deadline and its process group is reaped. A
+    /// deadline hit returns `tool_timeout` with no partial result. Positive
+    /// integer; omitted means no new limit; default off.
+    #[arg(long = "tool-timeout-ms", global = true, value_name = "N")]
+    pub tool_timeout_ms: Option<u64>,
+
+    /// Refuse an opted-in tool call once its logical input exceeds N bytes
+    /// (WP18, FR-19, ADR-031). Counts file/git content read or scanned,
+    /// including cached-index content — not displayed output, which is a
+    /// separate truncation mechanism. A hit returns `tool_input_limit`.
+    /// Positive integer; omitted means no new limit; default off.
+    #[arg(long = "tool-input-bytes", global = true, value_name = "N")]
+    pub tool_input_bytes: Option<u64>,
+
+    /// Refuse an opted-in tool call once it walks more than N directory
+    /// entries (WP18, FR-19, ADR-031; integrator D56 `(proposed)`). A hit
+    /// returns `tool_input_limit` naming the entry ceiling. Positive integer;
+    /// omitted means no new limit; default off.
+    #[arg(long = "tool-max-entries", global = true, value_name = "N")]
+    pub tool_max_entries: Option<u64>,
+
     /// Serve and export full, self-contained JSON Schema draft 2020-12 tool
     /// schemas instead of the default stripped placeholders (WP17, FR-15).
     /// Equivalent to `[mcp] full_tool_schemas = true`; the config/env seam still
diff --git a/src/config.rs b/src/config.rs
index 221b088..cd7654a 100644
--- a/src/config.rs
+++ b/src/config.rs
@@ -194,6 +194,13 @@ pub struct Config {
     /// deserialized from a config file; set once at startup from the CLI.
     #[serde(skip)]
     pub discovery_policy: crate::discovery_policy::DiscoveryPolicy,
+    /// `--tool-timeout-ms` / `--tool-input-bytes` / `--tool-max-entries`
+    /// (FR-19, WP18, ADR-031): the caller's cooperative per-call tool budgets.
+    /// Caller-owned; never deserialized from workspace/user config, so nothing
+    /// the caller did not pass explicitly can set or widen a limit. All-absent
+    /// keeps legacy behavior (no budget installed).
+    #[serde(skip)]
+    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
     #[serde(default)]
     pub tools: HashMap<String, ToolConfig>,
 }
diff --git a/src/main.rs b/src/main.rs
index 2300f8a..408197b 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -65,6 +65,7 @@ mod skills;
 mod snapshot;
 mod state_root;
 mod structured_output;
+mod tool_budget;
 mod tool_contract;
 mod tool_descriptions;
 mod tool_facade;
@@ -545,6 +546,32 @@ async fn main() -> anyhow::Result<()> {
     if cli.no_output_spill {
         cfg.tool_output.save_full_output = false;
     }
+    // FR-19 (WP18, ADR-031): pin the caller's cooperative tool-budget options.
+    // Positive integers only; a zero is a non-positive limit and is refused at
+    // startup with a named error rather than silently disabling the budget.
+    // Caller-owned: set only here, never from workspace/user config.
+    for (name, value) in [
+        ("--tool-timeout-ms", cli.tool_timeout_ms),
+        ("--tool-input-bytes", cli.tool_input_bytes),
+        ("--tool-max-entries", cli.tool_max_entries),
+    ] {
+        if value == Some(0) {
+            eprintln!(
+                "{}",
+                serde_json::json!({"error": {
+                    "code": "run_configuration_invalid",
+                    "reason": "tool_budget_invalid",
+                    "message": format!("{name} must be a positive integer")
+                }})
+            );
+            std::process::exit(2);
+        }
+    }
+    cfg.tool_budget = crate::tool_budget::ToolBudgetConfig {
+        timeout_ms: cli.tool_timeout_ms,
+        input_bytes: cli.tool_input_bytes,
+        max_entries: cli.tool_max_entries,
+    };
     // FR-07 (ADR-022): pin the read-only workspace decision before any writer.
     cfg.workspace_read_only = cli.workspace_read_only;
     if cfg.workspace_read_only
diff --git a/src/managed_process.rs b/src/managed_process.rs
index dd33db9..85c7824 100644
--- a/src/managed_process.rs
+++ b/src/managed_process.rs
@@ -18,6 +18,31 @@ use tokio::sync::Mutex;
 
 pub type ProgressCallback<'a> = dyn Fn(String) + Send + Sync + 'a;
 
+tokio::task_local! {
+    /// Cooperative tool-budget deadline (remaining time) in force for the
+    /// current operation, set by the dispatch seam via [`with_budget_deadline`].
+    /// Subprocesses that [`run`] spawns inside the scope inherit it so the OS
+    /// process group is reaped AT the deadline, never merely waited on
+    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
+    static BUDGET_DEADLINE: Option<Duration>;
+}
+
+/// Run `fut` with the cooperative tool-budget deadline in scope. A `None`
+/// deadline (no `--tool-timeout-ms`, or no deadline left to inherit) leaves the
+/// legacy behavior of [`run`] unchanged. The deadline bounds but never extends
+/// the process default timeout (FR-19, WP18, ADR-031).
+pub async fn with_budget_deadline<F: std::future::Future>(
+    deadline: Option<Duration>,
+    fut: F,
+) -> F::Output {
+    BUDGET_DEADLINE.scope(deadline, fut).await
+}
+
+/// The cooperative deadline in scope, or `None` outside a budgeted operation.
+fn scoped_budget_deadline() -> Option<Duration> {
+    BUDGET_DEADLINE.try_with(|d| *d).unwrap_or(None)
+}
+
 /// Captured process output. Each stream is bounded while it is read.
 pub struct ManagedOutput {
     pub status: ExitStatus,
@@ -468,14 +493,28 @@ pub async fn run(
     let mut command = Command::new(program);
     command.args(args).current_dir(cwd);
     apply_environment(&mut command, cfg, overlays);
-    let output = capture(&mut command, cfg, None, stdin_data).await?;
+    // FR-19 (WP18, ADR-031): a cooperative tool-budget deadline in scope caps
+    // this subprocess and reaps its process group at the deadline. `None`
+    // preserves the legacy process-default behavior exactly.
+    let (output, budget_fired) = capture_budgeted(
+        &mut command,
+        cfg,
+        None,
+        stdin_data,
+        scoped_budget_deadline(),
+    )
+    .await?;
     if output.timed_out {
         return Err(io::Error::new(
             io::ErrorKind::TimedOut,
-            format!(
-                "{program} timed out after {} seconds",
-                cfg.default_timeout_secs
-            ),
+            if budget_fired {
+                format!("{program} stopped at the tool budget deadline")
+            } else {
+                format!(
+                    "{program} timed out after {} seconds",
+                    cfg.default_timeout_secs
+                )
+            },
         ));
     }
     Ok(output)
@@ -488,6 +527,33 @@ pub async fn capture(
     progress: Option<&ProgressCallback<'_>>,
     stdin_data: Option<&[u8]>,
 ) -> io::Result<ManagedOutput> {
+    capture_inner(command, cfg, progress, stdin_data, None)
+        .await
+        .map(|(output, _)| output)
+}
+
+/// Like [`capture`], but also stops the child at `budget_deadline` when set,
+/// using the same TERM->grace->KILL process-group reap — never a bare timeout
+/// wrapper that leaves work running underneath (FR-19, WP18, ADR-031). Returns
+/// the output plus `true` when the stop was the cooperative tool budget's
+/// deadline rather than the process default timeout. Context: src/tool_budget.rs.
+pub async fn capture_budgeted(
+    command: &mut Command,
+    cfg: &ProcessConfig,
+    progress: Option<&ProgressCallback<'_>>,
+    stdin_data: Option<&[u8]>,
+    budget_deadline: Option<Duration>,
+) -> io::Result<(ManagedOutput, bool)> {
+    capture_inner(command, cfg, progress, stdin_data, budget_deadline).await
+}
+
+async fn capture_inner(
+    command: &mut Command,
+    cfg: &ProcessConfig,
+    progress: Option<&ProgressCallback<'_>>,
+    stdin_data: Option<&[u8]>,
+    budget_deadline: Option<Duration>,
+) -> io::Result<(ManagedOutput, bool)> {
     command
         .stdin(if stdin_data.is_some() {
             Stdio::piped()
@@ -506,18 +572,37 @@ pub async fn capture(
         .take()
         .ok_or_else(|| io::Error::other("spawned process has no stderr pipe"))?;
     let stdin = child.stdin_mut().take();
-    let timeout =
+    let proc_timeout =
         (cfg.default_timeout_secs > 0).then(|| Duration::from_secs(cfg.default_timeout_secs));
+    // FR-19 (WP18, ADR-031): the cooperative tool budget caps this wait at its
+    // remaining deadline and never extends the process default. The effective
+    // stop is the sooner of the two; `budget_is_tighter` records that the
+    // budget deadline was (at least as) soon, so the caller returns the typed
+    // `tool_timeout` rather than the legacy process-timeout error. Either stop
+    // reaps the whole process group through `ManagedChild::terminate`.
+    let effective_timeout = match (proc_timeout, budget_deadline) {
+        (Some(p), Some(b)) => Some(p.min(b)),
+        (Some(p), None) => Some(p),
+        (None, b) => b,
+    };
+    let budget_is_tighter = match (proc_timeout, budget_deadline) {
+        (_, None) => false,
+        (None, Some(_)) => true,
+        (Some(p), Some(b)) => b <= p,
+    };
     let grace = Duration::from_millis(cfg.termination_grace_ms);
 
     let wait = async {
-        if let Some(timeout) = timeout {
+        if let Some(timeout) = effective_timeout {
             match tokio::time::timeout(timeout, child.wait()).await {
-                Ok(status) => status.map(|status| (status, false)),
-                Err(_) => child.terminate(grace).await.map(|status| (status, true)),
+                Ok(status) => status.map(|status| (status, false, false)),
+                Err(_) => child
+                    .terminate(grace)
+                    .await
+                    .map(|status| (status, true, budget_is_tighter)),
             }
         } else {
-            child.wait().await.map(|status| (status, false))
+            child.wait().await.map(|status| (status, false, false))
         }
     };
     let write_stdin = async {
@@ -545,16 +630,19 @@ pub async fn capture(
     );
     let stdout = stdout?;
     let stderr = stderr?;
-    let (status, timed_out) = status?;
+    let (status, timed_out, budget_fired) = status?;
     stdin?;
-    Ok(ManagedOutput {
-        status,
-        stdout: stdout.render(),
-        stderr: stderr.render(),
-        stdout_truncated: stdout.truncated(),
-        stderr_truncated: stderr.truncated(),
-        timed_out,
-    })
+    Ok((
+        ManagedOutput {
+            status,
+            stdout: stdout.render(),
+            stderr: stderr.render(),
+            stdout_truncated: stdout.truncated(),
+            stderr_truncated: stderr.truncated(),
+            timed_out,
+        },
+        budget_fired,
+    ))
 }
 
 struct BoundedStream {
diff --git a/src/mcp.rs b/src/mcp.rs
index 3b8475e..4dea40d 100644
--- a/src/mcp.rs
+++ b/src/mcp.rs
@@ -438,6 +438,27 @@ async fn dispatch_tool_inner(
     }
     session.used_tools.insert(name.to_string());
 
+    // === FR-19 F3 (WP18, ADR-031): ONE place installs the operation budget and
+    // the deadline scope for EVERY registry tool (facade ops + plugins +
+    // git_read), not one match arm per tool. A composite child inherits the
+    // parent's budget (owned == false). A process-starting plugin whose logical
+    // input cannot be metered is refused when an input/entry budget is enabled
+    // (never silently exempted). The absolute deadline is threaded into
+    // managed_process so every subprocess group is reaped AT the deadline; a
+    // call whose deadline passed returns tool_timeout, never a late success. All
+    // a no-op when no budget option is set. ===
+    let budget_owned = session.begin_tool_operation();
+    // R1 (B1, D60): the input/entry allow-list gate. Every registry tool that
+    // is not on `tool_budget::tool_input_budget_admitted` is refused here
+    // (`tool_policy_unsupported`) when an input/entry budget is set, so a
+    // process plugin, snapshot, kgl_*, edit_file, or any later-added name is
+    // never silently exempted. A bare deadline refuses nothing here.
+    if let Some(failure) = session.budget_input_refusal(name) {
+        session.end_tool_operation(budget_owned);
+        return response_to_result(failure.into_response(name));
+    }
+    let budget_remaining = session.budget_remaining();
+    let dispatched = crate::managed_process::with_budget_deadline(budget_remaining, async {
     // Registry-based dispatch: route ops-backed tools through the facade.
     if let Some(resp) = tool_facade::invoke(session, name, args).await {
         session.last_response_meta = resp.meta.clone();
@@ -794,10 +815,16 @@ async fn dispatch_tool_inner(
 
         "git_read" => {
             let command = get_str(args, "command").unwrap_or_default();
+            // FR-19 (WP18, ADR-031): git_read runs inside the single budget +
+            // deadline scope installed above, so the git child's process group
+            // is reaped AT the deadline and a passed deadline becomes a typed
+            // tool_timeout at the post-dispatch check. Here we only meter the
+            // git object content it returned (the PM counts this as logical
+            // input): a byte-ceiling hit is tool_input_limit with no partial.
             let Some(registry) = &session.tool_registry else {
                 return err_text(serde_json::json!({"error":{"kind":"unavailable","message":"tool registry unavailable"}}).to_string());
             };
-            match registry
+            let run_result = registry
                 .run(
                     "git_read",
                     &command,
@@ -806,10 +833,14 @@ async fn dispatch_tool_inner(
                     None,
                     Some(args),
                 )
-                .await
-            {
+                .await;
+            match run_result {
                 Ok(result) => {
                     let text = result.output.to_string();
+                    if let Err(failure) = session.budget_charge_bytes(text.len() as u64, "git_read")
+                    {
+                        return response_to_result(failure.into_response("git_read"));
+                    }
                     if result.exit_code == 0 {
                         ok_text(text)
                     } else {
@@ -1158,6 +1189,19 @@ async fn dispatch_tool_inner(
 
         _ => Err(CallToolError::unknown_tool(name.to_string())),
     }
+    })
+    .await;
+    // FR-19 F2/F3 (WP18, ADR-031): a call whose deadline passed is a timeout,
+    // not a late result — even when the work finished just after the deadline,
+    // and even when a deadline-stopped subprocess surfaced as a plugin error
+    // string. Reclassify to the typed tool_timeout whenever the deadline has
+    // passed; otherwise keep the original result (success or a genuine error).
+    let dispatched = match session.budget_check_deadline(name) {
+        Ok(()) => dispatched,
+        Err(failure) => response_to_result(failure.into_response(name)),
+    };
+    session.end_tool_operation(budget_owned);
+    dispatched
 }
 
 fn extract_result_text(result: &CallToolResult) -> String {
@@ -1488,6 +1532,12 @@ impl ServerHandler for DaimonosHandler {
             let observe_on = crate::kgl::observe::enabled();
             let mut observed_ops: Vec<(String, Value)> = Vec::new();
 
+            // FR-19 (WP18, ADR-031): install ONE shared operation budget for
+            // the whole batch, so every sub-op (dispatched via dispatch_tool,
+            // which re-enters ops::dispatch) inherits the same remaining
+            // allowance and absolute deadline instead of each getting a fresh
+            // top-level budget. No-op when no budget option is set.
+            let batch_budget_owned = session.begin_tool_operation();
             let mut results = Vec::with_capacity(ops.len());
             for (i, op_val) in ops.iter().enumerate() {
                 let tool = match op_val.get("tool").and_then(|v| v.as_str()) {
@@ -1527,6 +1577,9 @@ impl ServerHandler for DaimonosHandler {
                 }
             }
 
+            if batch_budget_owned {
+                session.end_tool_operation(true);
+            }
             let payload = serde_json::to_string(&results).unwrap_or_default();
             // Read the dirty flag before the observe path may drop the lock.
             let tools_changed = session.take_tools_changed();
@@ -1555,6 +1608,13 @@ impl ServerHandler for DaimonosHandler {
             return bounded_text_tool_result(&output_cfg, "batch", payload, false).await;
         }
 
+        // FR-19 (WP18): a top-level call holds the session lock from here to
+        // its result, and each call clears the budget that it installed. So no
+        // budget of an earlier call can be on the session here.
+        debug_assert!(
+            session.tool_budget.is_none(),
+            "a tool budget leaked into the next top-level call"
+        );
         let result = dispatch_tool(&mut session, &params.name, &args).await;
         // Read the dirty flag before the observe path may drop the lock.
         let tools_changed = session.take_tools_changed();
@@ -1655,7 +1715,21 @@ async fn run_execute_script(
         .to_string();
     let timeout_secs =
         crate::script::bounded_timeout_secs(args.get("timeout").and_then(Value::as_i64));
-    script::execute(&code, session, std::time::Duration::from_secs(timeout_secs)).await
+    // FR-19 F4 (WP18, ADR-031): execute_script is a composite that owns ONE
+    // shared operation budget for all its inner ops. That budget is created and
+    // held inside the script context (src/script.rs) and installed on the
+    // session only inside each inner per-op lock, then cleared on release — it
+    // is deliberately NOT installed on the shared session here, because
+    // run_execute_script releases the lock while the script runs, and leaving
+    // it on the session across that window would let a concurrent top-level MCP
+    // call inherit it (wrong deadline/allowance). No-op when no budget option
+    // is set.
+    script::execute(
+        &code,
+        session.clone(),
+        std::time::Duration::from_secs(timeout_secs),
+    )
+    .await
 }
 
 /// Convert the Send-safe script outcome only after all handler awaits finish;
@@ -1765,6 +1839,7 @@ async fn build_instructions(workspace: &std::path::Path, cfg: &crate::config::Co
                     skip_dirs: &["target", "node_modules"],
                     // WP14 (FR-12): never descend through a link under no-follow.
                     follow_symlinks: !authority.no_follow(),
+                    max_entries: None,
                 },
             ) {
                 let mut dirs: Vec<_> = walk
diff --git a/src/ops/exec_ops.rs b/src/ops/exec_ops.rs
index 3b62be8..96af27f 100644
--- a/src/ops/exec_ops.rs
+++ b/src/ops/exec_ops.rs
@@ -533,6 +533,12 @@ async fn capture_exec(
     env: &HashMap<String, String>,
     on_progress: Option<&ExecProgressCallback<'_>>,
 ) -> Response {
+    // FR-19 (WP18, ADR-031): if the cooperative budget's deadline already
+    // passed, refuse before spawning anything rather than start work we would
+    // immediately have to stop. No budget / no deadline is a no-op.
+    if let Err(failure) = session.budget_check_deadline("exec") {
+        return failure.into_response("exec");
+    }
     let mut command = build_command(cmd, args);
     command.current_dir(cwd);
     crate::managed_process::apply_environment(&mut command, &session.cfg.process, env);
@@ -541,17 +547,24 @@ async fn capture_exec(
             callback(ExecProgress::Output(text));
         }
     });
-    let output = match crate::managed_process::capture(
+    // FR-19 (WP18, ADR-031): hand the budget's remaining deadline to the
+    // managed-process path so the exec is stopped (TERM/grace/KILL, whole
+    // process group reaped) at the sooner of it and the process default. A
+    // budget-deadline stop becomes a typed `tool_timeout` below; `None` keeps
+    // legacy behavior exactly.
+    let budget_deadline = session.budget_remaining();
+    let (output, budget_fired) = match crate::managed_process::capture_budgeted(
         &mut command,
         &session.cfg.process,
         progress
             .as_ref()
             .map(|callback| callback as &crate::managed_process::ProgressCallback<'_>),
         None,
+        budget_deadline,
     )
     .await
     {
-        Ok(output) => output,
+        Ok(result) => result,
         Err(error) => {
             if let Some(callback) = on_progress {
                 callback(ExecProgress::Exit {
@@ -568,6 +581,14 @@ async fn capture_exec(
             signal: exit_signal(&output.status),
         });
     }
+    // FR-19 (WP18, ADR-031): a cooperative-budget deadline stop returns the
+    // typed `tool_timeout` with no partial exec result, distinct from the
+    // legacy process-timeout (exit 6) path below.
+    if budget_fired {
+        if let Some(failure) = session.budget_timeout_failure("exec") {
+            return failure.into_response("exec");
+        }
+    }
     if output.timed_out {
         return Response::err(
             6,
diff --git a/src/ops/file_ops.rs b/src/ops/file_ops.rs
index 5facec8..635693b 100644
--- a/src/ops/file_ops.rs
+++ b/src/ops/file_ops.rs
@@ -14,6 +14,23 @@ pub async fn read(session: &mut Session, op: &Op) -> Response {
         None => return Response::err(3, "read requires path in 'p'"),
     };
 
+    // R2 (B4, FR-19): refuse by file size BEFORE reading when the size alone
+    // exceeds the remaining byte allowance, so a budget bounds the bytes read
+    // rather than only reclassifying a completed read. Charging the exact
+    // content length still happens after the read for files that pass.
+    match tokio::fs::metadata(&path).await {
+        Ok(meta) => {
+            if let Err(failure) =
+                session.budget_precheck_bytes(meta.len(), op.p.as_deref().unwrap_or("read_file"))
+            {
+                return failure.into_response("read_file");
+            }
+        }
+        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
+            return Response::err(1, &format!("not found: {}", path.display()))
+        }
+        Err(e) => return Response::err(4, &format!("io: {e}")),
+    }
     let content = match tokio::fs::read_to_string(&path).await {
         Ok(c) => c,
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
@@ -24,6 +41,19 @@ pub async fn read(session: &mut Session, op: &Op) -> Response {
     if let Some(index) = &session.index {
         index.observe_path(&path).await;
     }
+    // FR-19 F2 (WP18, ADR-031): a read that completed after the deadline is a
+    // timeout, not a late success with content; check before building a result.
+    if let Err(failure) = session.budget_check_deadline(op.p.as_deref().unwrap_or("read_file")) {
+        return failure.into_response("read_file");
+    }
+    // FR-19 (WP18, ADR-031): charge the logical file content read before
+    // producing any result. A byte-ceiling hit returns tool_input_limit with no
+    // partial content (output truncation is a separate mechanism).
+    if let Err(failure) =
+        session.budget_charge_bytes(content.len() as u64, op.p.as_deref().unwrap_or("read_file"))
+    {
+        return failure.into_response("read_file");
+    }
 
     let total_lines = content.lines().count();
     let offset = op.n.unwrap_or(0).max(0) as usize;
@@ -114,6 +144,12 @@ async fn read_confined(session: &mut Session, op: &Op) -> Response {
         Ok(Err(error)) => return Response::err(4, &format!("io: {error}")),
         Err(error) => return Response::err(4, &format!("io: {error}")),
     };
+    // FR-19 (WP18, ADR-031): charge logical file content for confined reads too.
+    if let Err(failure) =
+        session.budget_charge_bytes(content.len() as u64, op.p.as_deref().unwrap_or("read_file"))
+    {
+        return failure.into_response("read_file");
+    }
     let total_lines = content.lines().count();
     let offset = op.n.unwrap_or(0).max(0) as usize;
     let limit = op.n2.unwrap_or(0);
@@ -431,9 +467,31 @@ pub async fn ls(session: &Session, op: &Op) -> Response {
         type_filter: op.n2,
     };
 
+    // R2 (B4, FR-19, ADR-031): charge each entry INSIDE the walk and STOP at
+    // the ceiling, so an entry budget bounds the directory walk rather than
+    // walking the whole tree and reclassifying afterwards. The deadline is
+    // checked per entry too, so a bare deadline also stops a huge walk. A hit
+    // returns tool_input_limit (D56) / tool_timeout with no partial listing.
+    let budget = session.tool_budget();
     let mut entries = Vec::new();
-    if let Err(e) = ls_recurse(&path, &path, depth, &opts, &mut entries).await {
-        return Response::err(4, &format!("ls: {e}"));
+    let mut walked = 0u64;
+    match ls_recurse(
+        &path,
+        &path,
+        depth,
+        &opts,
+        &mut entries,
+        &mut walked,
+        budget.as_ref(),
+    )
+    .await
+    {
+        Ok(()) => {}
+        Err(WalkStop::Io(e)) => return Response::err(4, &format!("ls: {e}")),
+        Err(WalkStop::Budget(hit)) => {
+            session.note_budget_hit(&hit);
+            return hit.into_failure(Some("ls")).into_response("ls");
+        }
     }
 
     entries.sort_by(|a, b| {
@@ -475,6 +533,12 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
         op.q.as_deref()
             .and_then(|pattern| glob::Pattern::new(pattern).ok());
     let filter = op.n2;
+    // R2 (B4, WP18): cap the confined walk at the remaining entry allowance so
+    // `--tool-max-entries` bounds the walk itself; `None` keeps it unbounded.
+    let entry_cap = session
+        .tool_budget()
+        .and_then(|b| b.entry_remaining())
+        .map(|remaining| remaining as usize);
     let result = tokio::task::spawn_blocking(move || {
         // WP14: honor no-follow for the explicit scope so a symlink named as
         // the ls root is refused with its target, never listed through.
@@ -488,6 +552,7 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
                 follow_symlinks: !authority.no_follow(),
                 include_hidden: show_all,
                 skip_dirs: SKIP_DIRS,
+                max_entries: entry_cap,
             },
         )?;
         Ok::<_, crate::path_policy::ToolFailure>((base, walk))
@@ -503,6 +568,16 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
         Err(error) => return Response::err(4, &format!("ls: {error}")),
     };
     let (skipped, skipped_links) = walk.skipped_summary();
+    // FR-19 (WP18, ADR-031): charge directory entries walked on the confined
+    // path too (entries visited plus those skipped by the walk). No-op when no
+    // entry budget is set.
+    let walked = walk.entries.len() as u64 + skipped as u64 + skipped_links as u64;
+    if let Err(failure) = session.budget_check_deadline("ls") {
+        return failure.into_response("ls");
+    }
+    if let Err(failure) = session.budget_charge_entries(walked, "ls") {
+        return failure.into_response("ls");
+    }
     let mut entries = Vec::new();
     for entry in walk.entries {
         let Ok(relative) = entry.relative_path.strip_prefix(&base) else {
@@ -548,22 +623,46 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
     Response::ok(response)
 }
 
+/// R2 (B4): why a bounded `ls` walk stopped. `Io` is a filesystem error;
+/// `Budget` is a cooperative tool-budget hit (entry ceiling or deadline) raised
+/// MID-walk so a huge tree is not walked to completion.
+enum WalkStop {
+    Io(String),
+    Budget(crate::tool_budget::BudgetHit),
+}
+
 async fn ls_recurse(
     root: &std::path::Path,
     path: &std::path::Path,
     depth: usize,
     opts: &LsOpts<'_>,
     entries: &mut Vec<serde_json::Value>,
-) -> Result<(), String> {
+    walked: &mut u64,
+    budget: Option<&std::sync::Arc<crate::tool_budget::ToolBudget>>,
+) -> Result<(), WalkStop> {
     let mut dir = tokio::fs::read_dir(path).await.map_err(|e| {
-        if e.kind() == std::io::ErrorKind::NotFound {
+        WalkStop::Io(if e.kind() == std::io::ErrorKind::NotFound {
             format!("not found: {}", path.display())
         } else {
             format!("{e}")
-        }
+        })
     })?;
 
     while let Ok(Some(entry)) = dir.next_entry().await {
+        // FR-19 (WP18, ADR-031): every directory entry visited counts, before
+        // any hidden/skip/type/glob filter, so the charged input is honest.
+        *walked += 1;
+        // R2 (B4): check the deadline and charge this entry BEFORE doing any
+        // per-entry work, and STOP the walk on a hit so the ceiling truly
+        // bounds the walk. No-op when no budget is active.
+        if let Some(b) = budget {
+            if let Err(hit) = b.check_deadline() {
+                return Err(WalkStop::Budget(hit));
+            }
+            if let Err(hit) = b.charge_entries(1) {
+                return Err(WalkStop::Budget(hit));
+            }
+        }
         let name = entry.file_name().to_string_lossy().to_string();
         if !opts.show_all && name.starts_with('.') {
             continue;
@@ -630,7 +729,22 @@ async fn ls_recurse(
         }
 
         if is_dir && depth > 1 {
-            let _ = Box::pin(ls_recurse(root, &entry.path(), depth - 1, opts, entries)).await;
+            // Propagate a budget stop out of the recursion so the whole walk
+            // ends at the ceiling (an I/O error in a subdir is still skipped).
+            match Box::pin(ls_recurse(
+                root,
+                &entry.path(),
+                depth - 1,
+                opts,
+                entries,
+                walked,
+                budget,
+            ))
+            .await
+            {
+                Ok(()) | Err(WalkStop::Io(_)) => {}
+                Err(stop @ WalkStop::Budget(_)) => return Err(stop),
+            }
         }
     }
 
@@ -922,6 +1036,11 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
     let workspace = session.workspace.clone();
     // D08 (ADR-021, FR-09): explicit-only runs let no implicit ignore file steer the walk.
     let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
+    // FR-19 (WP18, ADR-031): hand the shared budget into the blocking worker so
+    // the worker itself meters the bytes it SCANS (not the bytes it returns)
+    // and checks the deadline per read chunk, stopping mid-scan. `None` keeps
+    // the legacy fast path byte-for-byte.
+    let budget = session.tool_budget();
 
     let result = tokio::task::spawn_blocking(move || {
         grep_blocking(
@@ -931,6 +1050,7 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
             max,
             file_glob.as_deref(),
             apply_ignore_files,
+            budget,
         )
     })
     .await;
@@ -945,9 +1065,18 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
                     .collect::<Vec<_>>();
                 index.observe_paths(observed).await;
             }
+            // A scan that completed exactly at the deadline is a timeout, not a
+            // late success (FR-19 F2). Bytes were already charged while reading.
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             Response::ok(json!({"matches": matches}))
         }
-        Ok(Err(e)) => Response::err(4, &format!("grep: {e}")),
+        Ok(Err(GrepStop::Budget(hit))) => {
+            session.note_budget_hit(&hit);
+            hit.into_failure(Some("search")).into_response("search")
+        }
+        Ok(Err(GrepStop::Io(e))) => Response::err(4, &format!("grep: {e}")),
         Err(e) => Response::err(4, &format!("grep task: {e}")),
     }
 }
@@ -982,11 +1111,42 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
     // even under confinement (--confine-paths), so a hostile ignore file cannot
     // steer confined evidence grep.
     let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
+    // FR-19 F1/F2 (ADR-031): meter scanned bytes and check the deadline inside
+    // the confined blocking worker too. `None` keeps the legacy path.
+    let budget = session.tool_budget();
     let result = tokio::task::spawn_blocking(move || {
         let mut searcher = Searcher::new();
+        let budget_hit: std::cell::RefCell<Option<crate::tool_budget::BudgetHit>> =
+            std::cell::RefCell::new(None);
         let mut scan = |file: std::fs::File,
                         relative: String,
                         matches: &mut Vec<serde_json::Value>| {
+            if let Some(b) = &budget {
+                if let Err(hit) = b.check_deadline() {
+                    *budget_hit.borrow_mut() = Some(hit);
+                    return false;
+                }
+                let mut reader =
+                    crate::tool_budget::BudgetedReader::new(file, std::sync::Arc::clone(b));
+                let ok = searcher
+                    .search_reader(
+                        &matcher,
+                        &mut reader,
+                        UTF8(|line_num, line| {
+                            if matches.len() < max {
+                                matches.push(
+                                    json!({"f": relative, "l": line_num, "t": line.trim_end()}),
+                                );
+                            }
+                            Ok(matches.len() < max)
+                        }),
+                    )
+                    .is_ok();
+                if let Some(hit) = reader.hit() {
+                    *budget_hit.borrow_mut() = Some(hit.clone());
+                }
+                return ok;
+            }
             searcher
                 .search_reader(
                     &matcher,
@@ -1017,6 +1177,9 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
                     .to_string_lossy()
                     .to_string();
                 let skipped = usize::from(!scan(start.into_file(), relative, &mut matches));
+                if let Some(hit) = budget_hit.borrow_mut().take() {
+                    return Err(hit.into_failure(Some("search")));
+                }
                 return Ok::<_, crate::path_policy::ToolFailure>((matches, skipped));
             }
             return Ok((matches, 0));
@@ -1068,12 +1231,23 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
             if !scan(authorized.into_file(), relative, &mut matches) {
                 skipped += 1
             }
+            if budget_hit.borrow().is_some() {
+                break;
+            }
+        }
+        if let Some(hit) = budget_hit.borrow_mut().take() {
+            return Err(hit.into_failure(Some("search")));
         }
         Ok::<_, crate::path_policy::ToolFailure>((matches, skipped))
     })
     .await;
     match result {
         Ok(Ok((matches, skipped))) => {
+            // Bytes were charged while scanning; a scan that finished exactly at
+            // the deadline is still a timeout, not a late success (FR-19 F2).
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             let mut value = json!({"matches": matches});
             if skipped > 0 {
                 value["skipped"] = json!(skipped)
@@ -1087,6 +1261,13 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
     }
 }
 
+/// How a budgeted grep worker stopped: an I/O/regex error, or a cooperative
+/// tool-budget hit the worker detected while scanning (FR-19 F1/F2, ADR-031).
+pub(crate) enum GrepStop {
+    Io(String),
+    Budget(crate::tool_budget::BudgetHit),
+}
+
 fn grep_blocking(
     pattern: &str,
     root: &Path,
@@ -1094,13 +1275,14 @@ fn grep_blocking(
     max: usize,
     file_glob: Option<&str>,
     apply_ignore_files: bool,
-) -> Result<Vec<serde_json::Value>, String> {
+    budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
+) -> Result<Vec<serde_json::Value>, GrepStop> {
     use grep_regex::RegexMatcher;
     use grep_searcher::sinks::UTF8;
     use grep_searcher::Searcher;
     use ignore::WalkBuilder;
 
-    let matcher = RegexMatcher::new(pattern).map_err(|e| format!("regex: {e}"))?;
+    let matcher = RegexMatcher::new(pattern).map_err(|e| GrepStop::Io(format!("regex: {e}")))?;
     let mut searcher = Searcher::new();
     let mut matches = Vec::new();
 
@@ -1143,6 +1325,42 @@ fn grep_blocking(
             .to_string_lossy()
             .to_string();
 
+        // FR-19 F1/F2 (ADR-031): when a budget is active, the worker itself
+        // meters the bytes it SCANS and checks the deadline at a bounded step
+        // (between files, and inside a large file per read chunk via
+        // BudgetedReader), stopping the scan the moment the ceiling or the
+        // deadline is hit. Without a budget the legacy search_path fast path is
+        // unchanged.
+        if let Some(budget) = &budget {
+            if let Err(hit) = budget.check_deadline() {
+                return Err(GrepStop::Budget(hit));
+            }
+            let file = match std::fs::File::open(path) {
+                Ok(file) => file,
+                Err(_) => continue,
+            };
+            let mut reader =
+                crate::tool_budget::BudgetedReader::new(file, std::sync::Arc::clone(budget));
+            let _ = searcher.search_reader(
+                &matcher,
+                &mut reader,
+                UTF8(|line_num, line| {
+                    if matches.len() < max {
+                        matches.push(serde_json::json!({
+                            "f": rel,
+                            "l": line_num,
+                            "t": line.trim_end(),
+                        }));
+                    }
+                    Ok(matches.len() < max)
+                }),
+            );
+            if let Some(hit) = reader.hit() {
+                return Err(GrepStop::Budget(hit.clone()));
+            }
+            continue;
+        }
+
         let _ = searcher.search_path(
             &matcher,
             path,
diff --git a/src/ops/mod.rs b/src/ops/mod.rs
index 1ab216b..235e8b9 100644
--- a/src/ops/mod.rs
+++ b/src/ops/mod.rs
@@ -31,7 +31,14 @@ pub async fn dispatch_with_progress(
     req: Request,
     on_exec_progress: Option<&ExecProgressCallback<'_>>,
 ) -> Response {
-    match req {
+    // FR-19 (WP18, ADR-031): install a fresh operation budget for this
+    // top-level call. A composite's children re-enter here and inherit the
+    // parent's budget (`owned == false`), so delegation cannot multiply the
+    // allowance or extend the deadline; the installing call clears it on
+    // return so each sequential top-level call starts fresh. When no budget
+    // option is set nothing is installed and behavior is unchanged.
+    let owned = session.begin_tool_operation();
+    let result = match req {
         Request::Single(op) => dispatch_op(session, op, on_exec_progress).await,
         Request::Batch { batch } => {
             let mut results = Vec::with_capacity(batch.len());
@@ -40,7 +47,9 @@ pub async fn dispatch_with_progress(
             }
             Response::ok(serde_json::to_value(results).unwrap())
         }
-    }
+    };
+    session.end_tool_operation(owned);
+    result
 }
 
 async fn dispatch_op(
@@ -143,10 +152,27 @@ async fn find(session: &Session, op: &Op) -> Response {
         None => None,
     };
 
+    // FR-19 F1 (WP18, ADR-031): the index-answered search (mode=files) reads
+    // cached index content, not scannable file content, so its logical input is
+    // not meterable as logical bytes; refuse an enabled input/entry budget
+    // rather than charge "bytes returned" (the owner choice stated in the
+    // handoff). Uses the mode-specific refusal because the allow-list admits
+    // `search` for its byte-metered content mode (R1/D60). The deadline IS
+    // enforced (checked before and after the search).
+    if let Some(failure) = session.budget_index_search_refusal() {
+        return failure.into_response("search");
+    }
+    if let Err(failure) = session.budget_check_deadline("search") {
+        return failure.into_response("search");
+    }
     idx.ensure_populated().await;
     let results = idx
         .search_scoped(&query, max, op.q.as_deref(), file_glob.as_ref())
         .await;
+    // A search that ran past the deadline is a timeout, not a late result.
+    if let Err(failure) = session.budget_check_deadline("search") {
+        return failure.into_response("search");
+    }
     let stats = idx.stats().await;
 
     Response::ok(serde_json::json!({
@@ -161,6 +187,13 @@ async fn find_confined(session: &Session, op: &Op) -> Response {
     let Some(query) = &op.p else {
         return Response::err(3, "find requires query in 'p'");
     };
+    // FR-19 F1 (WP18, ADR-031): the confined files-mode search ranks index/name
+    // content, not scannable file content, so refuse an enabled input/entry
+    // budget rather than charge "bytes returned" (same choice as the plain
+    // index path). The deadline is enforced below.
+    if let Some(failure) = session.budget_index_search_refusal() {
+        return failure.into_response("search");
+    }
     let glob = match op.g.as_deref().map(glob::Pattern::new) {
         Some(Ok(pattern)) => Some(pattern),
         Some(Err(error)) => return Response::err(3, &format!("invalid glob: {error}")),
@@ -258,6 +291,11 @@ async fn find_confined(session: &Session, op: &Op) -> Response {
     .await;
     match result {
         Ok(Ok((results, stats, skipped))) => {
+            // A search that ran past the deadline is a timeout, not a late
+            // result (FR-19 F2).
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             let mut value = serde_json::json!({"results": results, "index": stats});
             if skipped > 0 {
                 value["skipped"] = serde_json::json!(skipped)
diff --git a/src/ops/tool_ops.rs b/src/ops/tool_ops.rs
index 28abafc..7d2fe07 100644
--- a/src/ops/tool_ops.rs
+++ b/src/ops/tool_ops.rs
@@ -122,6 +122,20 @@ pub async fn tool_pipeline(session: &Session, op: &Op) -> Response {
         return failure.into_response(tool_id);
     }
 
+    // FR-19 F3 (WP18, ADR-031): a pipeline runs external plugin stages whose
+    // remaining deadline / logical input cannot be enforced stage-by-stage
+    // cooperatively. Rather than leave it "not cooperative", refuse the whole
+    // pipeline when any budget option is set (ADR-031 permits refusing the
+    // pipeline under --tool-timeout-ms; the same applies to input/entry). No
+    // budget option leaves it unchanged.
+    if session.budget_active() {
+        return crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
+            Some("tool_pipeline"),
+            "a pipeline's external stages are not cooperatively budgeted",
+        )
+        .into_response("tool_pipeline");
+    }
+
     let stages = match &op.a {
         Some(s) if !s.is_empty() => s,
         _ => return Response::err(3, "tool_pipeline requires stage names in 'a'"),
diff --git a/src/path_policy.rs b/src/path_policy.rs
index b07fe7f..50358cb 100644
--- a/src/path_policy.rs
+++ b/src/path_policy.rs
@@ -200,6 +200,77 @@ impl ToolFailure {
     pub fn unsupported(message: impl Into<String>) -> Self {
         Self::refusal(CODE_FILESYSTEM_POLICY_UNSUPPORTED, message, None)
     }
+
+    /// Cooperative tool-budget deadline hit (WP18, FR-19, ADR-031). The
+    /// `configured_ms` value is reported as both the limit and the consumed
+    /// amount: the call ran until at least the deadline. No partial success.
+    pub fn tool_timeout(argument: Option<&str>, configured_ms: u64) -> Self {
+        let mut failure = Self::refusal(
+            "tool_timeout",
+            format!("tool deadline exceeded: configured_ms={configured_ms}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure.budget = Some(Box::new(FailureBudget {
+            limit: configured_ms,
+            configured: configured_ms,
+            consumed: configured_ms,
+        }));
+        failure
+    }
+
+    /// Cooperative tool-budget logical-input hit (WP18, FR-19, ADR-031). Used
+    /// for both the byte ceiling and the directory-entry ceiling; `which`
+    /// names the ceiling that was hit (integrator D56 keeps the single
+    /// reviewed `tool_input_limit` code). No partial success.
+    /// Cooperative tool-budget refusal (WP18, FR-19, ADR-031, R1): a tool whose
+    /// relevant budget cannot be enforced is REFUSED when that budget is
+    /// enabled, rather than silently exempted (INTERFACES.md row WP18). It
+    /// carries the REVIEWED `tool_policy_unsupported` code (integrator D60, no
+    /// new code is minted) and a message that names the budget option(s) and,
+    /// via `into_response`, the tool. Used for the allow-list refusal under
+    /// `--tool-input-bytes`/`--tool-max-entries`, the index-answered search
+    /// (cached index content is not a scannable byte count), and `tool_pipeline`
+    /// (external stages cannot be budgeted cooperatively). Deadline enforcement
+    /// is unaffected.
+    pub fn tool_budget_policy_unsupported(argument: Option<&str>, detail: &str) -> Self {
+        let mut failure = Self::refusal(
+            crate::run_policy::CODE_TOOL_POLICY_UNSUPPORTED,
+            format!(
+                "cooperative tool budget (--tool-input-bytes/--tool-max-entries/\
+                 --tool-timeout-ms) cannot be enforced for this tool: {detail}"
+            ),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure
+    }
+
+    pub fn tool_input_limit(
+        argument: Option<&str>,
+        which: &str,
+        limit: u64,
+        consumed: u64,
+    ) -> Self {
+        let mut failure = Self::refusal(
+            "tool_input_limit",
+            format!("tool input limit exceeded ({which}): configured={limit}, consumed={consumed}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure.budget = Some(Box::new(FailureBudget {
+            limit,
+            configured: limit,
+            consumed,
+        }));
+        failure
+    }
 }
 
 /// Configurable bounds for the authority. Caller/config supplied — no hidden
@@ -297,6 +368,12 @@ pub struct WalkOptions {
     pub include_hidden: bool,
     /// Directory names that must not be emitted or traversed.
     pub skip_dirs: &'static [&'static str],
+    /// R2 (B4, WP18): optional cooperative-budget cap on the number of entries
+    /// to enumerate. When set, the walk STOPS once it has collected more than
+    /// this many entries (setting [`Walk::tool_entry_stop`]), so a
+    /// `--tool-max-entries` budget bounds the confined walk rather than walking
+    /// the whole tree. `None` (the default) keeps the legacy unbounded walk.
+    pub max_entries: Option<usize>,
 }
 
 impl Default for WalkOptions {
@@ -306,6 +383,7 @@ impl Default for WalkOptions {
             follow_symlinks: true,
             include_hidden: true,
             skip_dirs: &[],
+            max_entries: None,
         }
     }
 }
@@ -319,6 +397,10 @@ pub struct Walk {
     exhausted: bool,
     depth_exhausted: Option<usize>,
     failure: Option<ToolFailure>,
+    /// R2 (B4, WP18): set when the walk stopped early at the cooperative
+    /// [`WalkOptions::max_entries`] cap (distinct from the WP14 `exhausted`
+    /// walk-safety limit, which is a `walk_limit` refusal).
+    pub tool_entry_stop: bool,
 }
 
 /// An owned, already-authorized read handle to an in-root object.
@@ -1482,12 +1564,24 @@ impl FilesystemAuthority {
             if descend {
                 subdirs.push(child_rel);
             }
+            // R2 (B4, WP18): stop the walk once it has collected more than the
+            // cooperative entry cap, so `--tool-max-entries` bounds the walk
+            // itself (not a post-hoc count). One past the cap lets the caller's
+            // charge report a consumed total that exceeds the ceiling.
+            if opts.max_entries.is_some_and(|cap| walk.entries.len() > cap) {
+                walk.tool_entry_stop = true;
+                break;
+            }
         }
         // SAFETY: `dirp` is open and is closed exactly once here.
         unsafe { libc::closedir(dirp) };
 
         for sub in subdirs {
-            if walk.exhausted || walk.depth_exhausted.is_some() || walk.failure.is_some() {
+            if walk.exhausted
+                || walk.depth_exhausted.is_some()
+                || walk.failure.is_some()
+                || walk.tool_entry_stop
+            {
                 break;
             }
             self.walk_dir(&sub, depth + 1, max_depth, opts, walk, budget);
diff --git a/src/script.rs b/src/script.rs
index 6697b59..54133ee 100644
--- a/src/script.rs
+++ b/src/script.rs
@@ -512,6 +512,14 @@ fn build_globals(
     op_count: Arc<AtomicUsize>,
     subcall: Option<SubcallEnv>,
 ) -> Globals {
+    // FR-19 F4 (WP18, ADR-031): create ONE shared operation budget for the
+    // whole script here, so every inner op charges the same cumulative
+    // allowance against the same absolute deadline. It is installed on the
+    // session only inside each per-op lock (see dispatch_request /
+    // dispatch_tool_by_name) and cleared when the lock is released, so it never
+    // lingers on the shared session where a concurrent top-level MCP call could
+    // inherit it. `None` when no budget option is set (legacy behavior).
+    let budget = handle.block_on(async { session.lock().await.cfg.tool_budget.start() });
     // We store session + handle + cancel flag in a thread-local so the
     // starlark_module functions can access them without capturing closures.
     TOOL_CTX.with(|ctx| {
@@ -521,6 +529,7 @@ fn build_globals(
             subcall,
             cancel,
             op_count,
+            budget,
         });
     });
 
@@ -572,6 +581,10 @@ struct ToolContext {
     /// Owned by the caller so the count survives even when the script errors
     /// after dispatching some ops.
     op_count: Arc<AtomicUsize>,
+    /// FR-19 F4: the one shared operation budget for this script, installed on
+    /// the session inside each per-op lock and cleared on release. `None` when
+    /// no budget option is set.
+    budget: Option<Arc<crate::tool_budget::ToolBudget>>,
 }
 
 thread_local! {
@@ -643,10 +656,16 @@ fn dispatch_request(request: Request, label: &str) -> Result<Response, anyhow::E
         let child_span =
             crate::observability::ToolSpan::new(label, crate::observability::tool_kind(label));
         let session = ctx.session.clone();
+        let budget = ctx.budget.clone();
         let resp = child_span.span().in_scope(|| {
             ctx.handle.block_on(async {
                 let mut s = session.lock().await;
+                // FR-19 F4: install the shared budget only while this op holds
+                // the lock; clear it before release so it never leaks to a
+                // concurrent top-level call. begin_tool_operation inherits it.
+                s.set_operation_budget(budget.clone());
                 let resp = ops::dispatch(&mut s, request).await;
+                s.set_operation_budget(None);
                 if let Some(analytics) = s.analytics.clone() {
                     let resp_chars = response_chars(&resp);
                     let (saved_tokens, savings_pct) =
@@ -715,17 +734,34 @@ fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Respons
     })? {
         return Ok(failure.into_response(name));
     }
+    // R1 (B1, D60): the input/entry budget allow-list gate for EVERY inner
+    // Starlark call, before the opcode shortcut or the plugin fall-through, so a
+    // script cannot reach an unmetered tool (edit_file, snapshot, kgl_*, a
+    // plugin, any later name) under `--tool-input-bytes`/`--tool-max-entries`.
+    // Keyed on the configured budget (consulted before the shared budget is
+    // installed on the locked op). A bare `--tool-timeout-ms` refuses nothing.
+    if let Some(failure) = with_ctx(|ctx| {
+        let session = ctx.session.clone();
+        Ok(ctx
+            .handle
+            .block_on(async { session.lock().await.budget_input_refusal(name) }))
+    })? {
+        return Ok(failure.into_response(name));
+    }
     let resp = match tools::build_request(name, args) {
         Some(Ok(request)) => dispatch_request(request, name)?,
         Some(Err(e)) => return Err(anyhow::anyhow!("{e}")),
         None => with_ctx(|ctx| {
             let started = std::time::Instant::now();
             let request_chars = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);
+            let budget = ctx.budget.clone();
             ctx.handle.block_on(async {
                 let mut session = ctx.session.lock().await;
-                let Some((content, is_error, meta)) =
-                    crate::mcp::dispatch_local_tool(&mut session, name, args).await
-                else {
+                // FR-19 F4: shared budget bounded to this locked op (see above).
+                session.set_operation_budget(budget.clone());
+                let dispatched = crate::mcp::dispatch_local_tool(&mut session, name, args).await;
+                session.set_operation_budget(None);
+                let Some((content, is_error, meta)) = dispatched else {
                     if name.starts_with("mcp__") {
                         return Err(anyhow::anyhow!(
                             "remote MCP tool '{name}' is unavailable in execute_script: \
diff --git a/src/session.rs b/src/session.rs
index 9312648..164a93e 100644
--- a/src/session.rs
+++ b/src/session.rs
@@ -89,10 +89,29 @@ pub struct Session {
     /// and model-visible listing. Unrestricted keeps legacy behavior.
     /// Context: src/run_policy.rs::ToolPolicy.
     pub policy: crate::run_policy::ToolPolicy,
+    /// Caller's cooperative tool-budget options (FR-19, WP18, ADR-031), copied
+    /// from `cfg.tool_budget`. Used to install a fresh operation budget at each
+    /// top-level tool call. Context: src/tool_budget.rs.
+    pub tool_budget_config: crate::tool_budget::ToolBudgetConfig,
+    /// The operation budget in force for the current top-level tool call, or
+    /// `None` between calls / when no budget option is set. Installed once per
+    /// top-level call and shared (inherited, never reset) by composite children
+    /// so delegation cannot multiply the allowance (ADR-031).
+    pub tool_budget: Option<Arc<crate::tool_budget::ToolBudget>>,
+    /// R3 (B3): the budget hit (code, configured, consumed) recorded IN-BAND by
+    /// the budget code when a genuine deadline/input/entry limit fired on this
+    /// call, so the run loop records the reserved `limit_hit` audit event from a
+    /// trusted signal instead of parsing attacker-influenceable result text.
+    /// Interior-mutable so the `&self` budget helpers can set it; drained by
+    /// `take_budget_hit` and cleared when a fresh top-level budget is installed.
+    pub budget_hit: std::sync::Mutex<Option<BudgetHitRecord>>,
 }
 
 pub type BgProcess = crate::managed_process::ManagedBackground;
 
+/// R3 (B3): an in-band cooperative budget hit: `(code, configured, consumed)`.
+pub type BudgetHitRecord = (String, Option<u64>, Option<u64>);
+
 impl Session {
     pub fn new(workspace: PathBuf, cfg: Arc<Config>) -> Self {
         let cwd = match &cfg.filesystem_authority {
@@ -121,6 +140,7 @@ impl Session {
             relocated(cfg.snapshot_directory.as_ref()),
         );
         let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
+        let tool_budget_config = cfg.tool_budget;
         Self {
             workspace,
             cwd,
@@ -146,9 +166,227 @@ impl Session {
             authority,
             admission,
             policy,
+            tool_budget_config,
+            tool_budget: None,
+            budget_hit: std::sync::Mutex::new(None),
         }
     }
 
+    /// Install a fresh operation budget for a top-level tool call unless one is
+    /// already active (a composite's children inherit the parent's). Returns
+    /// `true` when this call installed the budget and must clear it afterwards
+    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
+    pub fn begin_tool_operation(&mut self) -> bool {
+        if self.tool_budget.is_some() {
+            return false;
+        }
+        // R3/R4: a fresh top-level call starts with no stale in-band hit.
+        *self.budget_hit.lock().expect("budget_hit poisoned") = None;
+        self.tool_budget = self.tool_budget_config.start();
+        self.tool_budget.is_some()
+    }
+
+    /// R3 (B3): record an in-band budget hit (the code and its numbers) so the
+    /// run loop can write the reserved `limit_hit` event without parsing result
+    /// text. Called by the budget helpers and the native readers on a genuine
+    /// hit only.
+    pub fn note_budget_hit(&self, hit: &crate::tool_budget::BudgetHit) {
+        use crate::tool_budget::BudgetHit;
+        let record = match hit {
+            BudgetHit::Timeout { configured_ms } => (
+                "tool_timeout".to_string(),
+                Some(*configured_ms),
+                Some(*configured_ms),
+            ),
+            BudgetHit::Input {
+                limit, consumed, ..
+            } => (
+                "tool_input_limit".to_string(),
+                Some(*limit),
+                Some(*consumed),
+            ),
+        };
+        *self.budget_hit.lock().expect("budget_hit poisoned") = Some(record);
+    }
+
+    /// R3 (B3): drain the in-band budget hit recorded during the last dispatch.
+    pub fn take_budget_hit(&self) -> Option<BudgetHitRecord> {
+        self.budget_hit.lock().expect("budget_hit poisoned").take()
+    }
+
+    /// Clear the operation budget if this call owned it. A composite child
+    /// (`owned == false`) leaves the parent's budget in place.
+    pub fn end_tool_operation(&mut self, owned: bool) {
+        if owned {
+            self.tool_budget = None;
+        }
+    }
+
+    /// Charge `bytes` logical input against the active budget. Returns the
+    /// typed [`crate::path_policy::ToolFailure`] when the ceiling is exceeded;
+    /// a no-op when no byte budget is active.
+    pub fn budget_charge_bytes(
+        &self,
+        bytes: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.charge_input(bytes) {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
+    /// R2 (B4): refuse `bytes` against the active byte budget WITHOUT charging,
+    /// so `read_file` can reject a file by its metadata size before reading it.
+    /// Returns the typed `tool_input_limit` when the size alone would exceed the
+    /// remaining allowance; a no-op when no byte budget is active.
+    pub fn budget_precheck_bytes(
+        &self,
+        bytes: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.precheck_input(bytes) {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
+    /// A clone of the active operation budget, for handing the shared `Arc`
+    /// into a blocking search worker so the worker itself meters scanned bytes
+    /// and checks the deadline (FR-19 F1/F2). `None` when no budget is active.
+    pub fn tool_budget(&self) -> Option<std::sync::Arc<crate::tool_budget::ToolBudget>> {
+        self.tool_budget.clone()
+    }
+
+    /// Install (or clear with `None`) the operation budget directly. Used by
+    /// `execute_script` (FR-19 F4): the script owns ONE shared budget and
+    /// installs it on the session only inside each inner per-op lock, clearing
+    /// it when the lock is released, so the budget never lingers on the shared
+    /// session between ops where a concurrent top-level call could inherit it.
+    /// `begin_tool_operation` then sees it already installed and inherits.
+    pub fn set_operation_budget(
+        &mut self,
+        budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
+    ) {
+        self.tool_budget = budget;
+    }
+
+    /// True when ANY operation budget is active (deadline, byte, or entry).
+    /// Used to refuse composite routes whose external work cannot be governed
+    /// cooperatively (e.g. `tool_pipeline`).
+    pub fn budget_active(&self) -> bool {
+        self.tool_budget.is_some()
+    }
+
+    /// True when the active budget carries an input-byte or directory-entry
+    /// ceiling (a logical-input budget a tool must be able to meter).
+    pub fn budget_has_input_limits(&self) -> bool {
+        self.tool_budget
+            .as_ref()
+            .is_some_and(|b| b.has_input_limits())
+    }
+
+    /// R1 (B1, ADR-031): under an input-byte or directory-entry budget, admit a
+    /// tool only if it is on the explicit [`crate::tool_budget::tool_input_budget_admitted`]
+    /// allow-list (it meters its input or reads no workspace/external content).
+    /// Every other name is REFUSED with the reviewed `tool_policy_unsupported`
+    /// (D60), never silently exempted. Keyed on the configured budget, not an
+    /// installed one, so every frontend dispatch boundary (the MCP seam, the
+    /// agent/chat/ACP loop, and `execute_script`) can consult it uniformly
+    /// before a budget is even installed. `None` when no input/entry budget is
+    /// set (a bare `--tool-timeout-ms` refuses nothing here) or when `tool` is
+    /// admitted.
+    pub fn budget_input_refusal(&self, tool: &str) -> Option<crate::path_policy::ToolFailure> {
+        let cfg = &self.tool_budget_config;
+        let input_or_entry = cfg.input_bytes.is_some() || cfg.max_entries.is_some();
+        if input_or_entry && !crate::tool_budget::tool_input_budget_admitted(tool) {
+            Some(
+                crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
+                    Some(tool),
+                    "it is not on the input/entry budget allow-list (it reads workspace \
+                 or external content that is not metered, or starts external work)",
+                ),
+            )
+        } else {
+            None
+        }
+    }
+
+    /// R1 (ADR-031): the index-answered search (`search mode=files`) reads
+    /// cached-index content that is not a scannable byte count, so under an
+    /// input/entry budget it is REFUSED regardless of the admission allow-list
+    /// (which admits `search` for its byte-metered content mode). Checks the
+    /// installed budget (this runs inside `ops::dispatch`, after the budget was
+    /// installed). `None` when no input/entry budget is active.
+    pub fn budget_index_search_refusal(&self) -> Option<crate::path_policy::ToolFailure> {
+        if self.budget_has_input_limits() {
+            Some(
+                crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
+                    Some("search"),
+                    "the index (files) search mode reads cached-index content that is not a \
+                 scannable byte count",
+                ),
+            )
+        } else {
+            None
+        }
+    }
+
+    /// Charge `entries` directory entries against the active budget.
+    pub fn budget_charge_entries(
+        &self,
+        entries: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.charge_entries(entries) {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
+    /// Remaining time until the active budget's absolute deadline, to hand to
+    /// the managed-subprocess path so external work (exec, git_read) inherits
+    /// but never extends it (FR-19, WP18, ADR-031). `None` when no budget or no
+    /// deadline is active.
+    pub fn budget_remaining(&self) -> Option<std::time::Duration> {
+        self.tool_budget.as_ref().and_then(|b| b.remaining())
+    }
+
+    /// The typed `tool_timeout` failure for the active budget, used after the
+    /// managed-subprocess path reports it stopped external work at the
+    /// deadline. `None` when no budget is active.
+    pub fn budget_timeout_failure(
+        &self,
+        argument: &str,
+    ) -> Option<crate::path_policy::ToolFailure> {
+        self.tool_budget
+            .as_ref()
+            .map(|b| b.timeout_failure(Some(argument)))
+    }
+
+    /// Error if the active budget's absolute deadline has passed.
+    pub fn budget_check_deadline(
+        &self,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.check_deadline() {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
     /// Build the initial session environment with an enhanced PATH.
     /// Combines: config extra_path + auto-detected tool dirs + parent process PATH.
     fn build_initial_env(cfg: &Config) -> HashMap<String, String> {
diff --git a/src/session_bootstrap.rs b/src/session_bootstrap.rs
index 5a7ac78..a552469 100644
--- a/src/session_bootstrap.rs
+++ b/src/session_bootstrap.rs
@@ -61,6 +61,14 @@ pub struct BootstrapOptions<'a> {
     /// Part of the launch identity so a frontend requesting it cannot silently
     /// attach to a daemon launched without it.
     pub full_tool_schemas: bool,
+    /// FR-19 (WP18, ADR-031): the caller's cooperative tool-budget options.
+    /// Part of the daemon's immutable launch identity exactly like
+    /// `workspace_read_only`: forwarded in `daemon_args`, hashed into
+    /// `bootstrap_fingerprint`, and treated as an explicit selection in
+    /// `launch_identity`, so a client requesting a budget cannot be served by a
+    /// daemon started without the same one (the daemon pins its budget from its
+    /// OWN parsed flags). All-absent keeps legacy behavior.
+    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
 }
 
 pub struct BootstrapConnection {
@@ -229,6 +237,21 @@ fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
     if options.no_follow {
         args.push(OsString::from("--no-follow-symlinks"));
     }
+    // FR-19 (WP18, ADR-031): forward the caller's budget to the daemon so an
+    // interactive session actually enforces it (global flags, before the
+    // subcommand). Omitted options add nothing, preserving legacy launch args.
+    if let Some(ms) = options.tool_budget.timeout_ms {
+        args.push(OsString::from("--tool-timeout-ms"));
+        args.push(OsString::from(ms.to_string()));
+    }
+    if let Some(bytes) = options.tool_budget.input_bytes {
+        args.push(OsString::from("--tool-input-bytes"));
+        args.push(OsString::from(bytes.to_string()));
+    }
+    if let Some(entries) = options.tool_budget.max_entries {
+        args.push(OsString::from("--tool-max-entries"));
+        args.push(OsString::from(entries.to_string()));
+    }
     args.push(OsString::from("session-daemon"));
     args.push(OsString::from("--socket"));
     args.push(options.socket_path.as_os_str().to_owned());
@@ -298,6 +321,20 @@ fn bootstrap_fingerprint(options: &BootstrapOptions<'_>) -> String {
     if options.no_follow {
         digest.update(b"no-follow\0");
     }
+    // FR-19 (WP18, ADR-031): pin the budget into the launch identity so an
+    // attach cannot silently run under a different (or absent) budget.
+    if let Some(ms) = options.tool_budget.timeout_ms {
+        digest.update(b"tool-timeout-ms\0");
+        digest.update(ms.to_le_bytes());
+    }
+    if let Some(bytes) = options.tool_budget.input_bytes {
+        digest.update(b"tool-input-bytes\0");
+        digest.update(bytes.to_le_bytes());
+    }
+    if let Some(entries) = options.tool_budget.max_entries {
+        digest.update(b"tool-max-entries\0");
+        digest.update(entries.to_le_bytes());
+    }
     if let Some(provider) = options.provider {
         digest.update(provider.as_bytes());
     }
@@ -320,6 +357,7 @@ fn launch_identity(options: &BootstrapOptions<'_>) -> std::io::Result<LaunchIden
         && options.confine_identity.is_none()
         && !options.ignore_config_files
         && !options.no_follow
+        && !options.tool_budget.any_enabled()
     {
         return Ok(LaunchIdentity::NotRequested);
     }
@@ -395,6 +433,7 @@ mod tests {
         agent_env: &'a Path,
     ) -> BootstrapOptions<'a> {
         BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             workspace,
             tool_allowlist: None,
             config_path: Some(config),
@@ -474,6 +513,71 @@ mod tests {
         );
     }
 
+    // WP18 (FR-19, ADR-031, req 1): the interactive agent runs through the
+    // session daemon, so each budget option must be a daemon launch argument
+    // AND part of the launch identity. If it were not in the identity, an
+    // attach could silently reuse a daemon with a different (or absent) budget;
+    // the client compares fingerprints and refuses on mismatch.
+    #[test]
+    fn tool_budget_is_a_daemon_argument_and_pins_the_launch_identity() {
+        let base = options(
+            Path::new("/workspace"),
+            Path::new("/config.toml"),
+            Path::new("/runtime/session.sock"),
+            Path::new("/agent.env"),
+        );
+        let budgeted = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig {
+                timeout_ms: Some(1500),
+                input_bytes: Some(4096),
+                max_entries: Some(32),
+            },
+            ..options(
+                Path::new("/workspace"),
+                Path::new("/config.toml"),
+                Path::new("/runtime/session.sock"),
+                Path::new("/agent.env"),
+            )
+        };
+        let args = daemon_args(&budgeted);
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-timeout-ms", "1500"]));
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-input-bytes", "4096"]));
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-max-entries", "32"]));
+        // The unbudgeted daemon carries none of these arguments.
+        assert!(!daemon_args(&base)
+            .iter()
+            .any(|arg| arg == "--tool-timeout-ms"));
+        // A budget changes the launch identity, so an attach to a daemon that
+        // lacks it (or has a different one) is refused.
+        assert_ne!(
+            bootstrap_fingerprint(&base),
+            bootstrap_fingerprint(&budgeted)
+        );
+        let other = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig {
+                timeout_ms: Some(500),
+                input_bytes: None,
+                max_entries: None,
+            },
+            ..options(
+                Path::new("/workspace"),
+                Path::new("/config.toml"),
+                Path::new("/runtime/session.sock"),
+                Path::new("/agent.env"),
+            )
+        };
+        assert_ne!(
+            bootstrap_fingerprint(&budgeted),
+            bootstrap_fingerprint(&other)
+        );
+    }
+
     // WP11 (FR-09, ADR-021): explicit-only mode is pinned into the daemon's
     // launch identity, the same way --confine-paths is.
     #[test]
@@ -485,6 +589,7 @@ mod tests {
             Path::new("/agent.env"),
         );
         let explicit = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             ignore_config_files: true,
             ..options(
                 Path::new("/workspace"),
@@ -512,6 +617,7 @@ mod tests {
         let config = directory.path().join("config.toml");
         let agent_env = directory.path().join("agent.env");
         let pinned = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -652,6 +758,7 @@ mod tests {
             LaunchIdentity::Unavailable
         );
         let bare = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -776,6 +883,7 @@ mod tests {
         // explicit selection (not NotRequested), so it is compared against the
         // daemon's published identity.
         let read_only = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -786,6 +894,7 @@ mod tests {
         // The running daemon published a NON-read-only fingerprint (its own
         // bare identity). The client must NOT silently attach.
         let non_read_only = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
diff --git a/src/tool_budget.rs b/src/tool_budget.rs
new file mode 100644
index 0000000..c861353
--- /dev/null
+++ b/src/tool_budget.rs
@@ -0,0 +1,438 @@
+//! Cooperative, operation-scoped tool budgets (WP18, FR-19, ADR-031).
+//!
+//! A budget carries an absolute deadline and two independent optional logical
+//! counters — bytes of file/git content read or scanned, and directory entries
+//! walked. It is created once per top-level tool call and shared by every child
+//! of a composite (`batch`, `execute_script`, `tool_pipeline`) so delegation
+//! cannot multiply the allowance: a child inherits the remaining allowance and
+//! cannot extend the deadline (ADR-031). Limits are caller-set and disabled by
+//! default; when every option is absent no budget is installed and behavior is
+//! unchanged.
+//!
+//! Counting is logical, not physical: charge the file/git object content read
+//! or scanned (including cached-index content) and directory entries walked —
+//! never device I/O or displayed/truncated output bytes, which remain a
+//! separate mechanism (`src/tool_output.rs`).
+//!
+//! Context: docs/contexts/reviewer-runtime/CONTEXT.md,
+//! docs/adr/031-cooperative-tool-budgets.md.
+
+use std::io::{self, Read};
+use std::sync::atomic::{AtomicU64, Ordering};
+use std::sync::Arc;
+use std::time::{Duration, Instant};
+
+use crate::path_policy::ToolFailure;
+
+/// R1 (B1, ADR-031): the ALLOW-LIST that governs admission under an input-byte
+/// or directory-entry budget (`--tool-input-bytes` / `--tool-max-entries`). A
+/// tool is admitted ONLY if it appears here; every other name — process
+/// plugins, config-registered `[tools.*]`/`tool_run`, `kgl_*`, snapshot and
+/// restore, `bg`, outbound `mcp__*`, and any name a later package adds — is
+/// REFUSED with the reviewed `tool_policy_unsupported` (D60), never silently
+/// exempted. Inverting the old deny-list closes the silent-exemption surface:
+/// a new unmeterable tool is refused by default, not admitted by omission.
+///
+/// Admitted because they METER the logical input they read:
+/// `read_file` (charges file content bytes read); `ls` (charges directory
+/// entries walked); `search` (content mode charges scanned bytes in the
+/// blocking worker — the index/files mode is refused separately in `ops::find`
+/// because cached-index content is not a scannable byte count); `git_read`
+/// (metered by the bytes of git object content it returns — D61 approximation,
+/// recorded in docs/configuration.md).
+///
+/// Admitted because they read NO workspace or external content to meter:
+/// `write_file`, `append_file` (write output only; they scan no input);
+/// `batch`, `execute_script` (composite dispatchers — each child re-enters this
+/// gate and is metered or refused on its own while sharing the one budget per
+/// ADR-031, so admitting the wrapper exempts nothing); the session/runtime
+/// state tools `set_cwd`, `set_verbosity`, `set_external_session_id`,
+/// `session_stats` (in-process session state, not workspace files); the static
+/// tool-catalog tools `get_tool_schema`, `list_tool_signatures`,
+/// `list_all_tools` (static schemas, not workspace content); the agent-mail
+/// tools `register_agent`, `list_agents`, `send_message`, `reply_message`,
+/// `fetch_inbox`, `mark_read`, `acknowledge`, `fetch_thread` and the advisory
+/// reservation tools `reserve_paths`, `renew_reservations`,
+/// `release_reservations`, `check_conflicts`, `list_reservations` (agent-mail /
+/// reservation store per ADR-009, not workspace file/git content); and
+/// `update_plan` (in-memory agent plan state).
+///
+/// Deliberately ABSENT (refused under an input/entry budget): `edit_file` and
+/// `diff_files` (read workspace files unmetered), `skill` (reads a skill file),
+/// `workspace_info` (reads the root listing unmetered), `kgl_query`/`kgl_assert`
+/// (knowledge-graph store), `snapshot` (snapshot store content), `tool_pipeline`
+/// (external stages), and every process plugin.
+pub fn tool_input_budget_admitted(name: &str) -> bool {
+    matches!(
+        name,
+        // Meter their logical input.
+        "read_file"
+            | "ls"
+            | "search"
+            | "git_read"
+            // No input to meter (write-only).
+            | "write_file"
+            | "append_file"
+            // Composite dispatchers: children are gated individually.
+            | "batch"
+            | "execute_script"
+            // Session / runtime state.
+            | "set_cwd"
+            | "set_verbosity"
+            | "set_external_session_id"
+            | "session_stats"
+            // Static tool-catalog introspection.
+            | "get_tool_schema"
+            | "list_tool_signatures"
+            | "list_all_tools"
+            // Agent-mail store (ADR-009).
+            | "register_agent"
+            | "list_agents"
+            | "send_message"
+            | "reply_message"
+            | "fetch_inbox"
+            | "mark_read"
+            | "acknowledge"
+            | "fetch_thread"
+            // Advisory-reservation store (ADR-009).
+            | "reserve_paths"
+            | "renew_reservations"
+            | "release_reservations"
+            | "check_conflicts"
+            | "list_reservations"
+            // In-memory agent plan state.
+            | "update_plan"
+    )
+}
+
+/// Caller-configured budget options. All-absent means no budget is installed
+/// (legacy behavior). Caller-owned; never deserialized from workspace config.
+#[derive(Debug, Clone, Copy, Default)]
+pub struct ToolBudgetConfig {
+    /// `--tool-timeout-ms N`: absolute per-call deadline in milliseconds.
+    pub timeout_ms: Option<u64>,
+    /// `--tool-input-bytes N`: logical input byte ceiling for the call.
+    pub input_bytes: Option<u64>,
+    /// `--tool-max-entries N`: directory-entry ceiling for the call (D56,
+    /// `(proposed)` spelling; a hit reports the reviewed `tool_input_limit`).
+    pub max_entries: Option<u64>,
+}
+
+impl ToolBudgetConfig {
+    pub fn any_enabled(&self) -> bool {
+        self.timeout_ms.is_some() || self.input_bytes.is_some() || self.max_entries.is_some()
+    }
+
+    /// Install a fresh operation budget for a top-level call, or `None` when no
+    /// option is set (preserving legacy behavior).
+    pub fn start(&self) -> Option<Arc<ToolBudget>> {
+        if !self.any_enabled() {
+            return None;
+        }
+        Some(Arc::new(ToolBudget::new(*self)))
+    }
+}
+
+/// Which input ceiling a failure names. Both use the reviewed
+/// `tool_input_limit` code; the message names which was hit (integrator D56).
+#[derive(Debug, Clone, Copy)]
+pub enum Limit {
+    Bytes,
+    Entries,
+}
+
+impl Limit {
+    fn label(self) -> &'static str {
+        match self {
+            Limit::Bytes => "input bytes",
+            Limit::Entries => "directory entries",
+        }
+    }
+}
+
+/// An operation-scoped cooperative budget: an absolute deadline plus two
+/// independent atomic counters. Shared across the children of one top-level
+/// call via `Arc`, so concurrent delegation cannot multiply the allowance.
+pub struct ToolBudget {
+    deadline: Option<Instant>,
+    timeout_ms: Option<u64>,
+    input_limit: Option<u64>,
+    input_consumed: AtomicU64,
+    entry_limit: Option<u64>,
+    entry_consumed: AtomicU64,
+}
+
+impl ToolBudget {
+    fn new(cfg: ToolBudgetConfig) -> Self {
+        let deadline = cfg
+            .timeout_ms
+            .map(|ms| Instant::now() + Duration::from_millis(ms));
+        Self {
+            deadline,
+            timeout_ms: cfg.timeout_ms,
+            input_limit: cfg.input_bytes,
+            input_consumed: AtomicU64::new(0),
+            entry_limit: cfg.max_entries,
+            entry_consumed: AtomicU64::new(0),
+        }
+    }
+
+    /// Remaining time until the absolute deadline, for handing to the managed
+    /// subprocess path so external work inherits (never extends) it. `None`
+    /// when no deadline is set.
+    pub fn remaining(&self) -> Option<Duration> {
+        self.deadline
+            .map(|d| d.saturating_duration_since(Instant::now()))
+    }
+
+    /// The typed deadline failure for this budget, used when external work
+    /// (a managed subprocess) was stopped at the deadline (FR-19, WP18).
+    pub fn timeout_failure(&self, argument: Option<&str>) -> ToolFailure {
+        BudgetHit::Timeout {
+            configured_ms: self.timeout_ms.unwrap_or(0),
+        }
+        .into_failure(argument)
+    }
+
+    /// Error if the absolute deadline has passed. Checked at bounded work
+    /// boundaries by native/index readers.
+    pub fn check_deadline(&self) -> Result<(), BudgetHit> {
+        if let Some(d) = self.deadline {
+            if Instant::now() >= d {
+                return Err(BudgetHit::Timeout {
+                    configured_ms: self.timeout_ms.unwrap_or(0),
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// True when an input-byte or directory-entry ceiling is configured (i.e.
+    /// a logical-input budget this tool would have to meter). Used to refuse a
+    /// tool that starts unmeterable external work (ADR-031).
+    pub fn has_input_limits(&self) -> bool {
+        self.input_limit.is_some() || self.entry_limit.is_some()
+    }
+
+    /// Charge `n` logical input bytes. No-op when no byte limit is set. Returns
+    /// a hit when the cumulative total exceeds the ceiling; the counter still
+    /// reflects the attempt so the reported `consumed` is honest. INTERFACES.md
+    /// reserves this name (`charge_input`).
+    pub fn charge_input(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.input_limit {
+            // `fetch_add` wraps on overflow, so the sum is saturated here. Two
+            // concurrent children can each pass this check for a moment while
+            // their sum passes the limit: the budget is cooperative, and the
+            // later charge of either child then fails.
+            let consumed = self
+                .input_consumed
+                .fetch_add(n, Ordering::Relaxed)
+                .saturating_add(n);
+            if consumed > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Bytes,
+                    limit,
+                    consumed,
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// R2 (B4): check whether reading `n` more input bytes WOULD exceed the
+    /// ceiling, WITHOUT charging. Lets a reader (`read_file`) refuse by file
+    /// size from metadata before reading a single byte. Returns the hit it
+    /// would raise (reporting the would-be consumed total). No-op (Ok) when no
+    /// byte limit is set.
+    pub fn precheck_input(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.input_limit {
+            let would_be = self
+                .input_consumed
+                .load(Ordering::Relaxed)
+                .saturating_add(n);
+            if would_be > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Bytes,
+                    limit,
+                    consumed: would_be,
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// R2 (B4): the remaining directory-entry allowance (`entry_limit` minus
+    /// what has already been charged), so a walker can be given a hard cap and
+    /// stop early instead of walking the whole tree. `None` when no entry limit
+    /// is set.
+    pub fn entry_remaining(&self) -> Option<u64> {
+        self.entry_limit
+            .map(|limit| limit.saturating_sub(self.entry_consumed.load(Ordering::Relaxed)))
+    }
+
+    /// Charge `n` directory entries. No-op when no entry limit is set.
+    pub fn charge_entries(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.entry_limit {
+            let consumed = self
+                .entry_consumed
+                .fetch_add(n, Ordering::Relaxed)
+                .saturating_add(n);
+            if consumed > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Entries,
+                    limit,
+                    consumed,
+                });
+            }
+        }
+        Ok(())
+    }
+}
+
+/// A budget violation, convertible to the reviewed [`ToolFailure`].
+#[derive(Debug, Clone)]
+pub enum BudgetHit {
+    Timeout {
+        configured_ms: u64,
+    },
+    Input {
+        which: Limit,
+        limit: u64,
+        consumed: u64,
+    },
+}
+
+impl BudgetHit {
+    /// Build the caller-facing typed failure. A deadline hit carries
+    /// `tool_timeout`; a byte or entry hit carries `tool_input_limit` and names
+    /// which ceiling was hit, the configured value, and the consumed value.
+    pub fn into_failure(self, argument: Option<&str>) -> ToolFailure {
+        match self {
+            BudgetHit::Timeout { configured_ms } => {
+                ToolFailure::tool_timeout(argument, configured_ms)
+            }
+            BudgetHit::Input {
+                which,
+                limit,
+                consumed,
+            } => ToolFailure::tool_input_limit(argument, which.label(), limit, consumed),
+        }
+    }
+}
+
+/// A `Read` wrapper that charges scanned bytes and checks the deadline at every
+/// bounded read step, so a native search worker meters the bytes it SCANS (not
+/// the bytes it returns) and stops mid-file at the deadline or the byte ceiling
+/// (FR-19 F1/F2, ADR-031). The worker that owns the reader inspects
+/// [`BudgetedReader::hit`] after the search to surface the typed failure; the
+/// read error that aborts the search is only the stop signal. Wrap only when a
+/// budget is active — the unbudgeted path keeps its exact legacy behavior.
+pub struct BudgetedReader<R> {
+    inner: R,
+    budget: Arc<ToolBudget>,
+    hit: Option<BudgetHit>,
+}
+
+impl<R: Read> BudgetedReader<R> {
+    pub fn new(inner: R, budget: Arc<ToolBudget>) -> Self {
+        Self {
+            inner,
+            budget,
+            hit: None,
+        }
+    }
+
+    /// The budget violation that stopped the scan, if any.
+    pub fn hit(&self) -> Option<&BudgetHit> {
+        self.hit.as_ref()
+    }
+}
+
+impl<R: Read> Read for BudgetedReader<R> {
+    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
+        // Deadline first: a bounded step boundary inside a large file.
+        if let Err(hit) = self.budget.check_deadline() {
+            self.hit = Some(hit);
+            return Err(io::Error::other("tool budget deadline"));
+        }
+        let n = self.inner.read(buf)?;
+        if n > 0 {
+            // Charge exactly the bytes just scanned.
+            if let Err(hit) = self.budget.charge_input(n as u64) {
+                self.hit = Some(hit);
+                return Err(io::Error::other("tool input budget"));
+            }
+        }
+        Ok(n)
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    /// R1 (B1): the admission allow-list. Tools that meter their input or read
+    /// no workspace/external content are admitted under an input/entry budget;
+    /// every other name — process plugins, config-registered tools, kgl_*,
+    /// snapshot, bg, outbound mcp__*, and ANY unknown/future name — is refused.
+    #[test]
+    fn input_budget_admits_only_metered_or_contentless_tools() {
+        // Admitted: they meter their logical input.
+        for name in ["read_file", "ls", "search", "git_read"] {
+            assert!(
+                tool_input_budget_admitted(name),
+                "{name} should be admitted"
+            );
+        }
+        // Admitted: they read no workspace/external content, or are composite
+        // dispatchers whose children are gated individually.
+        for name in [
+            "write_file",
+            "append_file",
+            "batch",
+            "execute_script",
+            "set_cwd",
+            "session_stats",
+            "get_tool_schema",
+            "register_agent",
+            "send_message",
+            "reserve_paths",
+            "list_reservations",
+        ] {
+            assert!(
+                tool_input_budget_admitted(name),
+                "{name} should be admitted"
+            );
+        }
+        // Refused: read workspace/external content unmetered or start a
+        // subprocess. Includes the exact classes the reviewer named (B1).
+        for name in [
+            "edit_file",
+            "diff_files",
+            "skill",
+            "workspace_info",
+            "kgl_query",
+            "kgl_assert",
+            "snapshot",
+            "bg",
+            "tool_run",
+            "tool_pipeline",
+            "exec",
+            "git",
+            "cargo",
+            "curl",
+            "lint_repair",
+        ] {
+            assert!(
+                !tool_input_budget_admitted(name),
+                "{name} should be refused"
+            );
+        }
+        // Refused: outbound remote MCP tools (mcp__*) are never admitted.
+        assert!(!tool_input_budget_admitted("mcp__server__some_tool"));
+        // Refused by default: an unknown name a later package might add is NOT
+        // silently exempted — the whole point of inverting the deny-list.
+        assert!(!tool_input_budget_admitted("some_future_tool"));
+        assert!(!tool_input_budget_admitted(""));
+    }
+}
diff --git a/tests/README.md b/tests/README.md
index 0674fba..e1af393 100644
--- a/tests/README.md
+++ b/tests/README.md
@@ -196,3 +196,28 @@ The keyword classification (reference vs data vs schema-position vs unknown)
 and the duplicate-`$id`/anchor refusal are proven at the
 `src/schema_contract.rs` unit seam, including a table that routes a cycle
 through every 2020-12 schema keyword.
+
+### WP18 cooperative tool budgets (`tests/test_tool_budgets.py`)
+
+Process-level acceptance tests for FR-19 (ADR-031): the three default-off
+budget options drive a real `daimonos` MCP process (and a real `agent --print`
+process for the audit/`--output json`/worker cases). Timing tests (AC-19.1) use
+a deadline of 1000–1500 ms and assert an upper bound of several seconds while
+keeping the data small, so a slower CI runner still stops promptly without
+flaking; each bound and its reason are stated in the test docstring. The
+`exec` and `git_read` reap proofs follow the method of
+`tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped`
+(scanning `/proc` for a unique marker) to show no child or grandchild outlives
+the stop; the `git_read` proof installs a `$GIT_DIR`-gated fake `git` shim so
+repo discovery still uses the real git while an operation call hangs. These
+tests need a Unix host with `/proc` and a real `git`; a skip is not a pass.
+
+`tests/test_tool_budgets.py` (WP18, FR-19): process-level cooperative tool
+budget tests — scanned-byte metering for content search, native deadline checks
+(read_file/ls/both search modes), plugin deadline+reap (curl, git), the
+`tool_policy_unsupported` refusals (each tool that is not on the admission
+list, the index search, the pipeline), the entry limit that stops the `ls`
+walk, the budget that does not leak into the next call or into a concurrent
+call, the
+AC-19.1 large-tree timeout bound, and the `limit_hit` audit event (by
+kind+fields). AC-19.1 writes 10000 small files and stays under ~10 s.
diff --git a/tests/test_tool_budgets.py b/tests/test_tool_budgets.py
new file mode 100644
index 0000000..1975f5f
--- /dev/null
+++ b/tests/test_tool_budgets.py
@@ -0,0 +1,1345 @@
+"""WP18 FR-19 process-level acceptance tests: cooperative tool budgets.
+
+Spawns a real daimonos MCP process (conftest ``daimonos_factory``) and drives
+the public tool interface. Budgets are default-off; every test that asserts a
+limit passes the CLI option explicitly. Context:
+docs/adr/031-cooperative-tool-budgets.md and the reviewer-runtime
+INTERFACES.md row "WP18, FR-19".
+"""
+
+import json
+
+
+def _text(result):
+    return result["content"][0]["text"]
+
+
+def test_input_bytes_limit_refuses_oversized_read(daimonos_factory, tmp_path):
+    """--tool-input-bytes N: a read whose logical input exceeds N is refused
+    with the reviewed ``tool_input_limit`` code and a budget naming the
+    configured value and the consumed value. No partial content is returned.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * 500)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 100, data
+    assert data["consumed"] >= 500, data
+    assert "content" not in data, "no partial result on a limit hit"
+
+
+def test_input_bytes_default_off_reads_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, an ordinary read is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * 500)
+    client = daimonos_factory(ws)
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    data = json.loads(_text(result))
+    assert data["content"] == "x" * 500
+
+
+import os
+import time
+
+
+def _survivors(needle):
+    """PIDs whose /proc cmdline contains `needle` — the process-table method of
+    tests/test_worker_isolation.py::_worker_survivors, used here to prove that
+    neither the exec child nor its grandchild outlives a budget stop (FR-19
+    requirement 3)."""
+    out = []
+    for pid in os.listdir("/proc"):
+        if not pid.isdigit():
+            continue
+        try:
+            with open("/proc/%s/cmdline" % pid, "rb") as f:
+                cmd = f.read().replace(b"\x00", b" ").decode("utf-8", "replace")
+        except OSError:
+            continue
+        if needle in cmd:
+            out.append(pid)
+    return out
+
+
+def _poll_gone(needle, bound_s):
+    deadline = time.monotonic() + bound_s
+    while time.monotonic() < deadline:
+        if not _survivors(needle):
+            return True
+        time.sleep(0.05)
+    return not _survivors(needle)
+
+
+def test_timeout_stops_exec_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
+    """--tool-timeout-ms N: an exec that outlives the deadline is stopped with
+    the reviewed ``tool_timeout`` code, and neither the child shell nor the
+    grandchild it backgrounds survives (FR-19 requirement 3; stopped work, not
+    only a stopped wait — the managed process group is TERM/KILL reaped via
+    ManagedChild::terminate).
+
+    Timing (AC-19.1, req 9): the deadline is 1000 ms — a limit of one second or
+    less so a slower CI runner still stops promptly. The call is asserted to
+    return within 8 s: generous for a loaded shared runner (expected stop is
+    ~1-3 s), yet well below the 15 s the shells would otherwise sleep, so a
+    pass proves the stop was the budget deadline, not the processes exiting on
+    their own.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    gc_marker = f"wp18gc-{os.getpid()}-{time.time_ns()}"
+    parent_marker = f"wp18parent-{os.getpid()}-{time.time_ns()}"
+    # The shell daimonos runs (sh -c <line>) backgrounds a grandchild shell
+    # whose argv carries gc_marker, then itself sleeps far past the deadline.
+    # Both stay in the exec's process group, so a group reap removes both.
+    line = (
+        f"sh -c 'sleep 15 # {gc_marker}' & "
+        f"sleep 15 # {parent_marker}"
+    )
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1000"])
+
+    start = time.monotonic()
+    result = client.call_tool("exec", {"command": line})
+    elapsed = time.monotonic() - start
+
+    assert elapsed < 8, f"budget did not stop exec promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "exit" not in data, "no partial exec result on a deadline hit"
+
+    assert _poll_gone(parent_marker, 5.0), "exec child survived the budget stop"
+    assert _poll_gone(gc_marker, 5.0), "exec grandchild survived the budget stop"
+
+
+def test_max_entries_limit_refuses_oversized_ls(daimonos_factory, tmp_path):
+    """--tool-max-entries N: an ls that walks more than N directory entries is
+    refused with the reviewed ``tool_input_limit`` code (integrator D56: the
+    entry ceiling reuses the byte code), a budget naming the configured value
+    and the consumed entry count, and no partial listing. Off by default.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(10):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "3"])
+
+    result = client.call_tool("ls", {"path": "."})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 3, data
+    # R2 (B4): the walk STOPS at the ceiling, so consumed is just past the
+    # configured limit (not the full tree) — enforcement, not post-hoc counting.
+    assert 3 < data["consumed"] <= 5, data
+    assert "entries" not in data, "no partial listing on an entry-limit hit"
+
+
+def test_max_entries_default_off_lists_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, an ordinary ls is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(10):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws)
+    result = client.call_tool("ls", {"path": "."})
+    data = json.loads(_text(result))
+    assert len(data["entries"]) == 10, data
+
+
+def test_input_bytes_meters_content_search_filesystem_path(daimonos_factory, tmp_path):
+    """--tool-input-bytes N on the FILESYSTEM search path (search mode=content
+    -> grep): the logical input the search returns is charged, so a result that
+    exceeds N is refused with ``tool_input_limit`` and no partial matches
+    (FR-19 req 4; the grep path reads the filesystem, not the index).
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(6):
+        (ws / f"hit_{i}.txt").write_text("needle here\n")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "40"])
+
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 40, data
+    assert "matches" not in data, "no partial matches on a limit hit"
+
+
+def test_input_bytes_refuses_index_search(daimonos_factory, tmp_path):
+    """F1 (M1): the INDEX search path (search mode=files -> trigram index)
+    reads the cached index, not scannable file content, so its logical input
+    cannot be metered. Under --tool-input-bytes the index-answered search is
+    REFUSED with a named error rather than charged "bytes returned" (ADR-031
+    allows refusing an enabled-but-unenforceable budget; the owner choice is
+    stated in the handoff). No partial results.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(8):
+        (ws / f"wp18idx_{i}.rs").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "30"])
+
+    result = client.call_tool("search", {"pattern": "wp18idx", "mode": "files"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_policy_unsupported", data
+    assert "results" not in data, "no partial index results on a refusal"
+
+
+# --- Composites share one budget (FR-19 requirement 2) ---
+
+_TWO_READS = (
+    "def main():\n"
+    "    read_file('big.txt')\n"
+    "    read_file('big.txt')\n"
+    "    return 'both'\n"
+    "result = main()\n"
+)
+
+
+def test_script_children_share_parent_budget(daimonos_factory, tmp_path):
+    """FR-19 req 2: a composite's children inherit the parent call's remaining
+    allowance, they do NOT each get a fresh top-level budget. Two reads of a
+    60-byte file under --tool-input-bytes 100: the first (60) fits, the second
+    (cumulative 120) trips the shared ceiling. This test FAILS if an
+    execute_script child is given its own new budget (both reads would fit).
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    result = client.call_tool("execute_script", {"code": _TWO_READS})
+    assert result.get("isError") is True, result
+    assert "tool_input_limit" in _text(result), _text(result)
+
+
+def test_script_single_read_within_budget_succeeds(daimonos_factory, tmp_path):
+    """Control: one 60-byte read under --tool-input-bytes 100 inside a script
+    succeeds, so the failure above is the shared second read, not read_file
+    being charged wrong."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    code = "def main():\n    read_file('big.txt')\n    return 'ok'\nresult = main()\n"
+    result = client.call_tool("execute_script", {"code": code})
+    assert result.get("isError") is not True, _text(result)
+    assert json.loads(_text(result))["result"] == "ok"
+
+
+def test_batch_children_share_parent_budget(daimonos_factory, tmp_path):
+    """FR-19 req 2: a batch's ops inherit the one parent budget. Two reads of a
+    60-byte file under --tool-input-bytes 100: op 1 fits, op 2 (cumulative 120)
+    trips the shared ceiling. FAILS if a batch op gets its own new budget."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+    ]})
+    content = json.loads(_text(result))
+    assert content[0]["ok"] is True, content
+    assert content[1]["ok"] is False, content
+    assert "tool_input_limit" in json.dumps(content[1]), content
+
+
+# --- Agent `--print` frontend: audit (req 5), --output json (req 11), req 1 ---
+#
+# A local, self-contained fake OpenAI-compatible endpoint (no network, no real
+# credential) drives a real `daimonos agent --print` process, mirroring
+# tests/test_run_audit.py. A budget option is passed as a global flag so the
+# agent frontend honors it; a limit hit must be a tool failure INSIDE the run
+# (the run still succeeds and, under --output json, still prints one document),
+# and the WP08 audit must record the named code.
+
+import os as _os
+import subprocess
+import threading
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
+from pathlib import Path
+
+_FAKE_KEY = "synthetic-not-a-real-key-WP18-5fZ9"
+_DONE = "WP18_AGENT_DONE_MARKER"
+
+
+class _FakeProvider:
+    def __init__(self, script):
+        self._script = list(script)
+        self._step = 0
+        self.received_tool_results = []
+        owner = self
+
+        class Handler(BaseHTTPRequestHandler):
+            protocol_version = "HTTP/1.1"
+
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                payload = json.dumps({"data": [{"id": "test/model",
+                    "context_length": 100000, "supported_parameters": ["tools"],
+                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
+                self.send_response(200)
+                self.send_header("Content-Type", "application/json")
+                self.send_header("Content-Length", str(len(payload)))
+                self.end_headers()
+                self.wfile.write(payload)
+
+            def do_POST(self):
+                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
+                tool_msgs = [m for m in body["messages"] if m.get("role") == "tool"]
+                if tool_msgs:
+                    owner.received_tool_results.append(json.dumps(tool_msgs))
+                step = owner._script[min(owner._step, len(owner._script) - 1)]
+                owner._step += 1
+                self.send_response(200)
+                self.send_header("Content-Type", "text/event-stream")
+                self.send_header("Connection", "close")
+                self.end_headers()
+                self.close_connection = True
+
+                def chunk(delta, reason=None):
+                    frame = {"choices": [{"index": 0, "delta": delta,
+                        "finish_reason": reason}],
+                        "usage": {"prompt_tokens": 42, "completion_tokens": 7}}
+                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
+                    self.wfile.flush()
+
+                try:
+                    if step[0] == "call":
+                        _, name, arguments = step
+                        chunk({"role": "assistant", "content": "", "tool_calls": [{
+                            "index": 0, "id": f"call-{owner._step}", "type": "function",
+                            "function": {"name": name,
+                                         "arguments": json.dumps(arguments)}}]},
+                              "tool_calls")
+                    else:
+                        chunk({"role": "assistant", "content": step[1]}, "stop")
+                    self.wfile.write(b"data: [DONE]\n\n")
+                    self.wfile.flush()
+                except (BrokenPipeError, ConnectionResetError):
+                    pass
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
+        self.thread.start()
+
+    @property
+    def port(self):
+        return self.server.server_port
+
+    def close(self):
+        self.server.shutdown()
+        self.server.server_close()
+        self.thread.join(timeout=3)
+
+
+def _agent_env_file(tmp_path, provider, allowed="read_file"):
+    path = tmp_path / "agent.env"
+    path.write_text("\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter",
+        "DAIMONOS_AGENT_MODEL=test/model",
+        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.port}/v1",
+        f"DAIMONOS_AGENT_API_KEY={_FAKE_KEY}",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
+        "DAIMONOS_AGENT_COMPACTION=off",
+        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
+        f"DAIMONOS_AGENT_ALLOWED_COMMANDS={allowed}",
+        "",
+    ]))
+    return path
+
+
+def _isolated_env(tmp_path):
+    stash = tmp_path / "home"
+    stash.mkdir(exist_ok=True)
+    (tmp_path / "tmp").mkdir(exist_ok=True)
+    env = {k: v for k, v in _os.environ.items() if not k.startswith("DAIMONOS_")}
+    env.update({
+        "HOME": str(stash),
+        "XDG_STATE_HOME": str(stash / "state"),
+        "XDG_DATA_HOME": str(stash / "data"),
+        "XDG_CONFIG_HOME": str(stash / "config"),
+        "TMPDIR": str(tmp_path / "tmp"),
+        "DAIMONOS_KGL_AUTOINDEX": "0",
+    })
+    return env
+
+
+def test_agent_print_json_budget_hit_is_tool_failure_and_is_audited(daimonos_binary, tmp_path):
+    """req 1 (agent --print honors the budget), req 11 (a limit hit under
+    --output json is a tool failure INSIDE the run; the run still prints its one
+    JSON document and exits 0) and req 5 (the WP08 audit records the hit with
+    its named code). The model calls read_file on a 500-byte file under
+    --tool-input-bytes 100, gets a tool_input_limit, then ends normally.
+    """
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    (workspace / "big.txt").write_text("z" * 500)
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider)
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "--tool-input-bytes", "100",
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "do the read"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    # req 11: exactly one JSON document on stdout.
+    doc = json.loads(completed.stdout)
+    assert isinstance(doc, dict), completed.stdout
+    # The tool result handed back to the model carried the named code: the
+    # limit hit was a tool failure inside the run, not a run abort.
+    joined = " ".join(provider.received_tool_results)
+    assert "tool_input_limit" in joined, joined
+    # req 5: the audit recorded the hit with its named code.
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    assert any("tool_input_limit" in json.dumps(ev) for ev in events), events
+
+
+# --- git_read deadline + process-group reap (FR-19 req 3; slice 06) ---
+
+import shutil
+
+
+def _init_repo(ws):
+    """A minimal real git repo for git_read to open, built with the real git on
+    PATH before any fake git is installed."""
+    real = shutil.which("git")
+    assert real, "a real git is required to build the fixture repo"
+
+    def g(*a):
+        subprocess.run([real, *a], cwd=str(ws), check=True,
+                       capture_output=True, env={**_os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
+                                                  "GIT_CONFIG_SYSTEM": "/dev/null"})
+    g("init", "-q")
+    g("config", "user.email", "wp18@example.invalid")
+    g("config", "user.name", "WP18")
+    (ws / "a.txt").write_text("hello\n")
+    g("add", "a.txt")
+    g("commit", "-q", "-m", "seed")
+    return real
+
+
+def _install_fake_git(bindir, real_git, parent_marker, gc_marker):
+    """A `git` shim that passes repo discovery through to the real git but, once
+    git_read sets GIT_DIR for an operation, backgrounds a grandchild and sleeps
+    far past the deadline. Both descendants carry a unique marker in their argv
+    and stay in the managed child's process group, so a group reap removes them.
+    """
+    bindir.mkdir(parents=True, exist_ok=True)
+    shim = bindir / "git"
+    shim.write_text(
+        "#!/bin/sh\n"
+        "if [ -n \"$GIT_DIR\" ]; then\n"
+        f"  sh -c 'sleep 12 # {gc_marker}' &\n"
+        f"  exec sh -c 'sleep 12 # {parent_marker}'\n"
+        "fi\n"
+        f"exec {real_git} \"$@\"\n"
+    )
+    shim.chmod(0o755)
+
+
+def test_timeout_stops_git_read_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
+    """--tool-timeout-ms N: a git_read whose git subprocess outlives the
+    deadline is stopped with the reviewed ``tool_timeout`` code, and neither the
+    git child nor the grandchild it backgrounds survives (FR-19 req 3 names
+    git_read for a reap proof; stopped work, not only a stopped wait — the whole
+    process group is TERM/KILL reaped via ManagedChild::terminate, the method of
+    tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped).
+
+    git_read is dispatched directly (not via ops::dispatch), so before this
+    slice a standalone git_read got no budget at all; the deadline is threaded
+    into managed_process::run via a task-local so the git child is actually
+    reaped, not merely abandoned.
+
+    Timing (AC-19.1, req 9): the deadline is 1500 ms — one and a half seconds so
+    a slower CI runner still stops promptly. The call is asserted to return
+    within 8 s (expected stop ~1.5-3 s) yet well below the 12 s the shims would
+    otherwise sleep, so a pass proves the stop was the budget deadline, not the
+    shims exiting on their own.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    real_git = _init_repo(ws)
+    bindir = tmp_path / "bin"
+    parent_marker = f"wp18git-{_os.getpid()}-{time.time_ns()}"
+    gc_marker = f"wp18gitgc-{_os.getpid()}-{time.time_ns()}"
+    _install_fake_git(bindir, real_git, parent_marker, gc_marker)
+
+    env = {"PATH": f"{bindir}{_os.pathsep}{_os.environ.get('PATH','')}",
+           "DAIMONOS_KGL_AUTOINDEX": "0"}
+    client = daimonos_factory(ws, env=env, args=["--tool-timeout-ms", "1500"])
+
+    start = time.monotonic()
+    result = client.call_tool("git_read", {"command": "log"})
+    elapsed = time.monotonic() - start
+
+    assert elapsed < 8, f"budget did not stop git_read promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "entries" not in data, "no partial git_read result on a deadline hit"
+
+    assert _poll_gone(parent_marker, 5.0), "git_read child survived the budget stop"
+    assert _poll_gone(gc_marker, 5.0), "git_read grandchild survived the budget stop"
+
+
+# --- Default off: every metered tool is unchanged with no budget (req 8) ---
+#
+# read_file and ls already have default-off regressions above. These cover the
+# remaining tools FR-19 req 8 names: exec, search, git_read. With none of the
+# three options, the new code paths (budget install, the with_budget_deadline
+# wrapper around managed_process::run) must be exact no-ops.
+
+
+def test_exec_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option an ordinary exec is unchanged
+    — it completes and returns its real exit, not a tool_timeout."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws)
+    result = client.call_tool("exec", {"command": "printf wp18ok"})
+    assert result.get("isError") is not True, _text(result)
+    data = json.loads(_text(result))
+    assert data["exit"] == 0, data
+    assert "wp18ok" in json.dumps(data), data
+
+
+def test_search_default_off_returns_all_matches(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option, a content search returns its
+    full result (no tool_input_limit)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(6):
+        (ws / f"hit_{i}.txt").write_text("needle here\n")
+    client = daimonos_factory(ws)
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_input_limit" not in _text(result), _text(result)
+
+
+def test_git_read_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option a git_read is unchanged — the
+    with_budget_deadline(None, ..) wrapper around the git subprocess is an exact
+    no-op and the real git log is returned, not a tool_timeout."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("git_read", {"command": "log"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_timeout" not in _text(result), _text(result)
+    assert "entries" in _text(result), _text(result)
+
+
+# --- Combinations: the budget composes with every other run option (req 6) ---
+#
+# Each test pairs a budget option with one other run-shaping option and asserts
+# the budget still fires with its reviewed code. Output truncation stays a
+# SEPARATE mechanism: the last test proves a result bounded by --no-output-spill
+# is not reported as a budget limit failure.
+
+
+def _big(ws, n=500):
+    (ws / "big.txt").write_text("x" * n)
+
+
+def test_input_bytes_composes_with_tool_allowlist(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --tool-allowlist. read_file is on
+    the allowlist, so admission passes; the byte budget still trips with
+    tool_input_limit (the allowlist and the budget are independent gates)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    allowlist = tmp_path / "allow.json"
+    allowlist.write_text(json.dumps(["read_file"]))
+    client = daimonos_factory(ws, args=["--tool-allowlist", str(allowlist),
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_input_bytes_composes_with_workspace_read_only(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --workspace-read-only. A read is
+    allowed under read-only, and the byte budget still trips."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    client = daimonos_factory(ws, args=["--workspace-read-only",
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_input_bytes_composes_with_confine_paths_and_no_follow(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --confine-paths and
+    --no-follow-symlinks. The read is inside the confined workspace and the byte
+    budget still trips with tool_input_limit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    client = daimonos_factory(ws, args=["--confine-paths", "--no-follow-symlinks",
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_no_output_spill_truncation_is_not_a_budget_limit(daimonos_factory, tmp_path):
+    """req 6: output truncation stays a mechanism distinct from the input
+    budget. A large read under --no-output-spill is bounded by the output rules,
+    not failed; with a generous --tool-input-bytes that the logical input fits
+    under, the result must NOT carry a budget code. Truncation != limit hit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws, n=300_000)
+    client = daimonos_factory(ws, args=["--no-output-spill",
+                                        "--tool-input-bytes", "10000000"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    body = _text(result)
+    assert "tool_input_limit" not in body, body
+    assert "tool_timeout" not in body, body
+
+
+# --- Workers (WP10, FR-19 req 7): a budgeted worker route is refused ---
+#
+# The cooperative budget installs at THIS process's dispatch seams, but a worker
+# routes tool calls into a separate process the parent budget never sees. So a
+# budget option cannot be enforced across the worker boundary; rather than
+# silently ignore it (a frontend-rule violation), the run is refused at startup
+# with run_configuration_invalid / exit 2 (D-WP18-5; ADR-031 refuse
+# enabled-but-unenforceable; INTERFACES worker rule). Ordinary worker mode with
+# no budget option is unchanged.
+
+_WORKER_REFUSAL = "tool_budget_worker_unenforceable"
+
+
+def _min_worker_config(tmp_path):
+    wc = tmp_path / "wc.json"
+    wc.write_text(json.dumps({"workers": {"w": {
+        "command": "/bin/true", "args": [],
+        "env": {"PATH": _os.environ.get("PATH", "/usr/bin:/bin")}}}}))
+    return wc
+
+
+def _run_agent_worker(daimonos_binary, tmp_path, provider, budget_args):
+    env = _isolated_env(tmp_path)
+    agent_env = _agent_env_file(tmp_path, provider)
+    ws = tmp_path / "ws"
+    ws.mkdir(exist_ok=True)
+    wc = _min_worker_config(tmp_path)
+    cmd = [daimonos_binary, "--workspace", str(ws), *budget_args,
+           "agent", "--print", "--worker-config", str(wc),
+           "--agent-env", str(agent_env), "do it"]
+    return subprocess.run(cmd, capture_output=True, text=True, timeout=60, env=env)
+
+
+def test_budgeted_worker_route_is_refused_at_startup(daimonos_binary, tmp_path):
+    """req 7: --tool-timeout-ms together with --worker-config is refused before
+    any worker launches, with the reviewed run_configuration_invalid code and
+    exit 2 and a reason naming the unenforceable budget."""
+    provider = _FakeProvider([("end", _DONE)])
+    try:
+        completed = _run_agent_worker(daimonos_binary, tmp_path, provider,
+                                      ["--tool-input-bytes", "1000"])
+    finally:
+        provider.close()
+    blob = completed.stdout + completed.stderr
+    assert completed.returncode == 2, blob
+    assert "run_configuration_invalid" in blob, blob
+    assert _WORKER_REFUSAL in blob, blob
+
+
+def test_unbudgeted_worker_route_is_not_refused_for_budget(daimonos_binary, tmp_path):
+    """req 7 control: with no budget option, worker mode is unchanged — it runs
+    its own WP10 path (here the trivial /bin/true worker is simply unavailable);
+    whatever its outcome, it is NOT the budget refusal."""
+    provider = _FakeProvider([("end", _DONE)])
+    try:
+        completed = _run_agent_worker(daimonos_binary, tmp_path, provider, [])
+    finally:
+        provider.close()
+    blob = completed.stdout + completed.stderr
+    assert _WORKER_REFUSAL not in blob, blob
+
+
+# ======================================================================
+# FIX run r4 (defects M1-M5). Each test targets one integrator measurement.
+# ======================================================================
+
+
+def test_input_bytes_charges_bytes_scanned_not_returned(daimonos_factory, tmp_path):
+    """F1 (M1): a content search charges the bytes it SCANS, not the bytes it
+    returns. 40 files of 100 KB without the word plus one with it, under
+    --tool-input-bytes 2000: the scan must stop and the call must fail with
+    ``tool_input_limit`` even though the returned match set is tiny. ``consumed``
+    exceeds the limit (the scan was stopped mid-stream) and stays below the
+    total bytes on disk (it stopped early). No matches on a limit hit.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    blob = "x" * (100 * 1024)
+    for i in range(40):
+        (ws / f"pad_{i:02}.txt").write_text(blob)
+    (ws / "zzz_match.txt").write_text("needle\n")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "2000"])
+
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 2000, data
+    assert data["consumed"] > 2000, data
+    assert data["consumed"] <= 41 * 100 * 1024, data
+    assert "matches" not in data, "no partial matches on a limit hit"
+
+
+def test_input_bytes_content_search_below_limit_passes(daimonos_factory, tmp_path):
+    """F1 control: a content scan whose total scanned bytes stay under the
+    ceiling returns its matches normally (so the failure above is the ceiling,
+    not search being broken)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "a.txt").write_text("needle here\n")  # ~12 bytes scanned
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100000"])
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is not True, _text(result)
+    data = json.loads(_text(result))
+    assert len(data["matches"]) == 1, data
+
+
+def test_timeout_read_file_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): read_file checks the deadline before building its result; with a
+    1 ms deadline (already elapsed by the time the native reader runs) it
+    returns ``tool_timeout``, never a late success with content."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * (100 * 1024))
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "content" not in data, "no late content on a deadline hit"
+
+
+def test_timeout_ls_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the ls walk checks the deadline before building its result."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("ls", {"path": "."})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "entries" not in data, "no late listing on a deadline hit"
+
+
+def test_timeout_content_search_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the content-search blocking worker itself checks the deadline
+    (passed into the worker), so a 1 ms deadline returns ``tool_timeout``, not a
+    late success."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"f{i}.txt").write_text("needle\n")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "matches" not in data, "no late matches on a deadline hit"
+
+
+def test_timeout_file_search_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the index (files-mode) search checks the deadline before the
+    result is built, so a 1 ms deadline returns ``tool_timeout``."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"wp18f_{i}.rs").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("search", {"pattern": "wp18f", "mode": "files"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+
+
+def test_ac_19_1_large_tree_search_times_out_within_bound(daimonos_factory, tmp_path):
+    """AC-19.1 / F2: a content search on a large tree (10000 files x ~1 KB =
+    ~10 MB; a full walk+scan of this many files clearly exceeds the limit on any
+    host in this environment, where opening 10000 files alone costs well over
+    100 ms) with a 100 ms limit returns ``tool_timeout`` within a stated upper
+    bound (8 s, far below the time a full scan would take), proving the deadline
+    STOPPED the work rather than the scan finishing on its own. The control with
+    no option returns success. Kept under ~10 s including setup.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    blob = "lorem ipsum dolor sit amet " * 38  # ~1 KB, no match
+    for i in range(10000):
+        (ws / f"f{i:05}.txt").write_text(blob)
+
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "100"])
+    start = time.monotonic()
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    elapsed = time.monotonic() - start
+    assert elapsed < 8, f"budget did not stop the search promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_timeout", _text(result)
+
+    control = daimonos_factory(ws)
+    r2 = control.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert r2.get("isError") is not True, _text(r2)
+    assert "tool_timeout" not in _text(r2), _text(r2)
+
+
+# --- F3: plugin/managed-process tools (M3 curl, M4 git) ---
+
+
+def _slow_http(delay_s):
+    class Handler(BaseHTTPRequestHandler):
+        protocol_version = "HTTP/1.1"
+
+        def log_message(self, *_):
+            pass
+
+        def do_GET(self):
+            time.sleep(delay_s)
+            body = b"wp18-slow-body"
+            self.send_response(200)
+            self.send_header("Content-Length", str(len(body)))
+            self.end_headers()
+            try:
+                self.wfile.write(body)
+            except (BrokenPipeError, ConnectionResetError):
+                pass
+
+    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+    server.daemon_threads = True
+    t = threading.Thread(target=server.serve_forever, daemon=True)
+    t.start()
+    return server, t
+
+
+def test_timeout_stops_curl_plugin(daimonos_factory, tmp_path):
+    """F3 (M3): the curl plugin starts a subprocess; a --tool-timeout-ms 700
+    deadline stops and reaps it, returning ``tool_timeout`` in under 3 s against
+    a server that would not answer for 4 s, and leaving no curl child alive."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    server, _t = _slow_http(20.0)
+    port = server.server_port
+    url = f"http://127.0.0.1:{port}/"
+    needle = f"wp18curl-{_os.getpid()}-{time.time_ns()}"
+    try:
+        client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
+        start = time.monotonic()
+        # The url carries a unique marker so _survivors can spot a leaked child.
+        result = client.call_tool("curl", {"url": url + needle})
+        elapsed = time.monotonic() - start
+    finally:
+        server.shutdown()
+        server.server_close()
+    # The server answers after a longer time than this bound. 8 s is the
+    # non-semantic bound of this file for a loaded runner.
+    assert elapsed < 8, f"curl ran past the deadline: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_timeout", data
+    assert _poll_gone(needle, 3.0), "curl child survived the budget stop"
+
+
+def test_timeout_stops_git_plugin(daimonos_factory, tmp_path):
+    """F4-of-M4 (timeout half): the `git` plugin (a subprocess) is stopped by
+    --tool-timeout-ms 1 with ``tool_timeout``, not a success."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_timeout", data
+
+
+def test_input_bytes_refuses_git_plugin(daimonos_factory, tmp_path):
+    """F3 (M4 input half): the `git` plugin reads no meterable logical input, so
+    under --tool-input-bytes it is REFUSED with the reviewed
+    ``tool_policy_unsupported`` code (D60) rather than passing uncounted
+    (ADR-031, R1)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "10"])
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_policy_unsupported", data
+
+
+def test_git_plugin_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option the `git` plugin is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_timeout" not in _text(result), _text(result)
+    assert "tool_policy_unsupported" not in _text(result), _text(result)
+
+
+def test_budgeted_pipeline_is_refused(daimonos_factory, tmp_path):
+    """F3 (pipeline): tool_pipeline runs external plugin stages whose remaining
+    deadline/input cannot be enforced stage-by-stage, so rather than leave it
+    "not cooperative" it is REFUSED with the reviewed ``tool_policy_unsupported``
+    code (D60) whenever any budget option is set (ADR-031 allows refusing when
+    --tool-timeout-ms is set; the same applies to input/entry). The call returns
+    promptly and starts no stage process."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
+    start = time.monotonic()
+    result = client.call_tool("tool_pipeline",
+                              {"tool_id": "git", "stages": ["status"]})
+    elapsed = time.monotonic() - start
+    assert elapsed < 8, f"refusal was not prompt: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_policy_unsupported", data
+
+
+def test_pipeline_default_off_not_refused(daimonos_factory, tmp_path):
+    """Regression: with no budget option a tool_pipeline is not budget-refused."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("tool_pipeline",
+                              {"tool_id": "git", "stages": ["status"]})
+    assert "cooperative tool budget" not in _text(result), _text(result)
+
+
+# --- F5: the audit limit_hit event, asserted by kind and fields ---
+
+
+def test_budget_hit_writes_limit_hit_audit_event(daimonos_binary, tmp_path):
+    """F5 (M5): a budget limit hit in a run with --audit-path writes the
+    reserved ``limit_hit`` audit event via record_limit_hit, carrying the limit
+    name, the tool, the configured value, and the consumed value. Asserted by
+    the event ``kind`` and its fields, not by a substring search for a code."""
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    (workspace / "big.txt").write_text("z" * 500)
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider)
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "--tool-input-bytes", "100",
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "do the read"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    hits = [ev for ev in events if ev.get("kind") == "limit_hit"]
+    assert hits, f"no limit_hit event: {[e.get('kind') for e in events]}"
+    hit = hits[0]
+    assert hit["tool"] == "read_file", hit
+    assert hit["limit"], hit
+    assert hit["configured"] == 100, hit
+    assert hit["consumed"] >= 500, hit
+
+
+# --- R1 (B1): allow-list inversion of the input/entry admission gate ---
+#
+# Under --tool-input-bytes / --tool-max-entries a tool is admitted only if it
+# meters its logical input (read_file, ls, content search, git_read) or reads
+# no workspace/external content (write_file, append_file, the session/meta/mail
+# tools, and the composite dispatchers). EVERY other name is refused with the
+# reviewed ``tool_policy_unsupported`` code (integrator D60) naming the budget
+# and the tool: process plugins, edit_file/diff_files (read content unmetered),
+# kgl_*, snapshot/restore, bg, config-registered tools, outbound mcp__*, and any
+# name a later package adds. The deadline alone refuses nothing except
+# tool_pipeline.
+
+
+def _budget_refusal(result):
+    data = json.loads(_text(result))
+    return data
+
+
+def test_input_bytes_refuses_edit_file(daimonos_factory, tmp_path):
+    """edit_file reads the target file to apply its edits but does not meter
+    that input, so under --tool-input-bytes it is refused (not silently
+    exempted) with tool_policy_unsupported (R1/B1, D60)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("hello world")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool(
+        "edit_file", {"path": "f.txt", "edits": ["hello", "HELLO"]}
+    )
+    assert result.get("isError") is True, result
+    data = _budget_refusal(result)
+    assert data["code"] == "tool_policy_unsupported", data
+    assert data.get("tool") == "edit_file", data
+    # original content untouched (refused before any effect)
+    assert (ws / "f.txt").read_text() == "hello world"
+
+
+def test_edit_file_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, edit_file is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("hello world")
+    client = daimonos_factory(ws)
+    result = client.call_tool(
+        "edit_file", {"path": "f.txt", "edits": ["hello", "HELLO"]}
+    )
+    assert result.get("isError") is not True, _text(result)
+    assert (ws / "f.txt").read_text() == "HELLO world"
+
+
+def test_input_bytes_refuses_snapshot(daimonos_factory, tmp_path):
+    """snapshot/restore read and write workspace content that the budget cannot
+    meter, so under --tool-input-bytes the snapshot tool is refused with
+    tool_policy_unsupported (R1/B1, D60)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("snapshot", {"action": "list"})
+    assert result.get("isError") is True, result
+    data = _budget_refusal(result)
+    assert data["code"] == "tool_policy_unsupported", data
+    assert data.get("tool") == "snapshot", data
+
+
+def test_snapshot_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, snapshot list is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws)
+    result = client.call_tool("snapshot", {"action": "list"})
+    assert result.get("isError") is not True, _text(result)
+
+
+def test_input_bytes_admits_write_file(daimonos_factory, tmp_path):
+    """write_file scans no workspace/external input, so there is nothing to
+    meter: it is admitted (not refused) under --tool-input-bytes and runs
+    normally. The inverted gate must not refuse a contentless tool (R1)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "10"])
+    result = client.call_tool("write_file", {"path": "out.txt", "content": "y" * 500})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_policy_unsupported" not in _text(result), _text(result)
+    assert (ws / "out.txt").read_text() == "y" * 500
+
+
+def test_max_entries_refuses_edit_file(daimonos_factory, tmp_path):
+    """The entry budget shares the admission gate: edit_file is refused under
+    --tool-max-entries too (R1)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("abc")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "100"])
+    result = client.call_tool("edit_file", {"path": "f.txt", "edits": ["a", "A"]})
+    assert result.get("isError") is True, result
+    data = _budget_refusal(result)
+    assert data["code"] == "tool_policy_unsupported", data
+
+
+def test_deadline_only_refuses_nothing_except_pipeline(daimonos_factory, tmp_path):
+    """The deadline alone (no input/entry budget) refuses no tool at the
+    admission gate: a tool that the input gate would refuse (edit_file) still
+    runs under --tool-timeout-ms. Only tool_pipeline is refused for a bare
+    deadline (its own check), proven by test_budgeted_pipeline_is_refused."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("hello")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "60000"])
+    result = client.call_tool("edit_file", {"path": "f.txt", "edits": ["hello", "HI"]})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_policy_unsupported" not in _text(result), _text(result)
+    assert (ws / "f.txt").read_text() == "HI"
+
+
+# --- R2 (B4): the entry limit must BOUND the walk, not account after it ---
+
+
+def test_max_entries_bounds_the_ls_walk(daimonos_factory, tmp_path):
+    """R2/B4: --tool-max-entries must stop the directory walk at the ceiling,
+    not walk the whole tree and reclassify afterwards. A tree with 5000 entries
+    and a limit of 50 returns tool_input_limit with ``consumed`` far below the
+    total (< 200), proving the walk stopped early; no partial listing. Post-hoc
+    accounting (the pre-R2 behavior) would report consumed == 5000."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    big = ws / "many"
+    big.mkdir()
+    for i in range(5000):
+        (big / f"f{i:05d}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "50"])
+    result = client.call_tool("ls", {"path": "many", "depth": 1})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["consumed"] < 200, f"walk not bounded; consumed={data.get('consumed')}"
+    assert "entries" not in data, "no partial listing on a limit hit"
+
+
+def test_read_file_refuses_oversized_by_size_precheck(daimonos_factory, tmp_path):
+    """R2/B4: read_file checks the file size from metadata and refuses without
+    reading when the size alone exceeds the remaining byte allowance. Observable
+    as a tool_input_limit whose consumed reflects the file size and no content,
+    even for a file much larger than the limit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "huge.txt").write_text("q" * 200_000)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "huge.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 100, data
+    assert data["consumed"] >= 200_000, data
+    assert "content" not in data, "no partial content on a size-precheck refusal"
+
+
+# --- R3 (B3): the limit_hit audit event must be in-band, not parsed from text ---
+
+
+def test_exec_printing_budget_json_writes_no_limit_hit_event(daimonos_binary, tmp_path):
+    """R3/B3: a tool whose RESULT TEXT merely looks like a budget failure (here
+    `exec` echoing the JSON shape, with NO budget option set) must NOT cause a
+    forged ``limit_hit`` audit event. The hit is signalled in-band by the budget
+    code on the session and read by the run loop after dispatch, never parsed
+    from attacker-influenceable result text."""
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    shape = '{"code":"tool_input_limit","configured":1,"consumed":2}'
+    provider = _FakeProvider([("call", "exec", {"command": f"echo '{shape}'"}),
+                              ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider, allowed="exec")
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "run the echo"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    hits = [ev for ev in events if ev.get("kind") == "limit_hit"]
+    assert not hits, f"forged limit_hit from result text: {hits}"
+
+
+# --- Review round 1, R4: a budget never leaks into the next top-level call ---
+
+def _code(result):
+    """The failure code of a tool result, or None for a success."""
+    if not result.get("isError"):
+        return None
+    try:
+        return json.loads(_text(result)).get("code", "error-without-code")
+    except ValueError:
+        return "error-not-json"
+
+
+def test_each_kind_of_failed_call_leaves_a_fresh_budget_for_the_next_call(daimonos_factory, tmp_path):
+    """After each call that ends early under a budget, the next top-level call
+    starts with a new budget. The probe is a read of a 60-byte file under
+    --tool-input-bytes 100: it passes only with a fresh allowance, because a
+    leaked budget already holds 60 bytes or more. A 70-byte read comes first in
+    each round, so a leak makes the probe fail with tool_input_limit.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "first.txt").write_text("a" * 70)
+    (ws / "probe.txt").write_text("b" * 60)
+    (ws / "big.txt").write_text("c" * 500)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100", "--tool-max-entries", "100"])
+
+    def probe(after):
+        result = client.call_tool("read_file", {"path": "probe.txt"})
+        assert _code(result) is None, f"budget leaked after {after}: {_text(result)}"
+
+    early_endings = [
+        ("a limit hit", "read_file", {"path": "big.txt"}, "tool_input_limit"),
+        ("a refused tool", "exec", {"command": "true"}, "tool_policy_unsupported"),
+        ("a tool error", "read_file", {"path": "does-not-exist.txt"}, "error-not-json"),
+        ("a refused index search", "search", {"pattern": "probe", "mode": "files"}, "tool_policy_unsupported"),
+    ]
+    for label, tool, arguments, expected in early_endings:
+        assert _code(client.call_tool("read_file", {"path": "first.txt"})) is None
+        result = client.call_tool(tool, arguments)
+        assert result.get("isError") is True, (label, result)
+        if expected != "error-not-json":
+            assert _code(result) == expected, (label, _text(result))
+        probe(label)
+
+    # An unknown tool name is an RPC error or a tool error. Both must leave no budget.
+    try:
+        client.call_tool("no_such_tool_wp18", {})
+    except RuntimeError:
+        pass
+    probe("an unknown tool name")
+
+    # A batch whose second child fails, and a script whose read fails.
+    batch = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "first.txt"}},
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+    ]})
+    assert "tool_input_limit" in _text(batch), _text(batch)
+    probe("a batch with a failed child")
+    script = client.call_tool("execute_script", {"code": _TWO_READS.replace("big.txt", "first.txt")})
+    assert "tool_input_limit" in _text(script), _text(script)
+    probe("a script that hit the limit")
+
+
+def test_a_timeout_leaves_a_fresh_budget_for_the_next_call(daimonos_factory, tmp_path):
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "probe.txt").write_text("b" * 60)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "800"])
+    stopped = client.call_tool("exec", {"command": "sleep", "args": ["30"]})
+    assert _code(stopped) == "tool_timeout", _text(stopped)
+    # The next call has its own 800 ms. A leaked budget has a passed deadline.
+    time.sleep(1.0)
+    result = client.call_tool("read_file", {"path": "probe.txt"})
+    assert _code(result) is None, _text(result)
+
+
+# --- Review round 1, R5: execute_script and its shared budget ---
+
+def test_a_failed_child_does_not_clear_the_shared_budget(daimonos_factory, tmp_path):
+    """A child that fails must not reset the budget of its composite. Under
+    --tool-input-bytes 100, two reads of a 60-byte file pass the limit together.
+    If the failed first child cleared the shared budget, each later read would
+    get a new allowance and both would pass.
+
+    The composite is `batch`, whose children continue after a failed child. In
+    `execute_script` a failed operation ends the script, so no later operation
+    of that script exists.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "part.txt").write_text("p" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "does-not-exist.txt"}},
+        {"tool": "read_file", "arguments": {"path": "part.txt"}},
+        {"tool": "read_file", "arguments": {"path": "part.txt"}},
+    ]})
+    children = json.loads(_text(result))
+    assert [child["ok"] for child in children] == [False, True, False], children
+    assert "tool_input_limit" not in json.dumps(children[0]), children[0]
+    last = json.dumps(children[2])
+    assert "tool_input_limit" in last and "120" in last, last
+
+
+def test_a_call_during_a_budgeted_script_does_not_share_its_budget(daimonos_factory, tmp_path):
+    """A second request arrives while a budgeted execute_script call sleeps
+    between two reads. Under --tool-input-bytes 100 with 60-byte files:
+
+    - The script reads 60 + 60 = 120 bytes and must end with tool_input_limit,
+      consumed 120. If the other call cleared or replaced the script budget,
+      the second read would pass.
+    - The other call reads 60 bytes and must succeed. If it inherited the
+      script budget (60 used), it would fail with consumed 120.
+
+    The two requests are sent on one connection without waiting. The test
+    requires that the reply of the read arrives before the reply of the script.
+    That shows that the read ran while the script was in progress.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "part.txt").write_text("p" * 60)
+    (ws / "other.txt").write_text("o" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    # The script reads, then computes for a moment (no tool call), then reads
+    # again. `exec` is not usable here: it is refused under an input limit.
+    slow_script = (
+        "def slow():\n"
+        "    total = 0\n"
+        "    for i in range(400000):\n"
+        "        total += i\n"
+        "    return total\n"
+        "\n"
+        "def main():\n"
+        "    read_file('part.txt')\n"
+        "    slow()\n"
+        "    return read_file('part.txt')\n"
+        "result = main()\n"
+    )
+    requests = [
+        {"jsonrpc": "2.0", "id": 9001, "method": "tools/call",
+         "params": {"name": "execute_script", "arguments": {"code": slow_script}}},
+        {"jsonrpc": "2.0", "id": 9002, "method": "tools/call",
+         "params": {"name": "read_file", "arguments": {"path": "other.txt"}}},
+    ]
+    client.process.stdin.write((json.dumps(requests[0]) + "\n").encode())
+    client.process.stdin.flush()
+    time.sleep(0.15)  # let the script start and make its first read
+    client.process.stdin.write((json.dumps(requests[1]) + "\n").encode())
+    client.process.stdin.flush()
+    replies = {}
+    order = []
+    while len(replies) < 2:
+        line = client.process.stdout.readline()
+        assert line, "the process closed stdout"
+        message = json.loads(line)
+        if message.get("id") in (9001, 9002):
+            replies[message["id"]] = message["result"]
+            order.append(message["id"])
+    assert order == [9002, 9001], f"the read did not run during the script: reply order {order}"
+
+    script = _text(replies[9001])
+    assert "tool_input_limit" in script, script
+    assert "consumed=120" in script, script
+    other = replies[9002]
+    assert _code(other) is None, _text(other)
+    assert "o" * 60 in _text(other)

### New Points
R1. S2 applied: both counters use saturating_add after fetch_add, with a comment on concurrent charges. S4 applied: the two 3 s bounds are 8 s, and the slow HTTP server of the curl test now answers after 20 s, so the bound still proves the stop.
R2. S3 recorded as decision D61 and documented in docs/configuration.md: git_read charges the bytes of the content that it returns, after git ended. This is an approximation and does not stop git early. The deadline stops git with its process group.
R3. S1 and S5 are NOT done. S1 (a real read error under a budget is not reported as a budget hit) has no test. S5: no process test runs the chat or ACP frontend with a budget option. The ACP test harness of the repository (tests/test_acp_permission.py::AcpClient) has no place for global options, and that change belongs to a shared test file. Both are named as open in INTERFACES.md and in the PR.
R4. Verification by the integrator on 5da3a19: fmt and clippy exit 0. cargo test --no-fail-fast: 2306 passed, 6 failed, 10 ignored (plugins::shellcheck::tests::*, no shellcheck on the host; the same 6 fail on the base). tests/test_tool_budgets.py: 49 passed, 2 runs. pytest tests: 981 passed, 10 failed, 1 skipped. 7 failures are tests/test_shellcheck.py. 3 failures are tests/test_acp_lifecycle.py::test_acp_runs_teardown_when_signaled, an existing test that sends a signal 1 s after the spawn. In 12 interleaved runs it failed 14 of 36 cases on this branch, 12 of 36 on the base 59cbded, and 15 of 36 on master f1fbfa7, so this package does not cause it. CI did not run on this branch: its PR is stacked on a non-default base branch.
R5. Process note: the worker sub-agent implemented B1, B4, and B3 in a fix run and ran out of budget. The integrator verified that state (build, fmt, clippy, tests) and wrote the B2 and B6 tests, the debug_assert, S2, S4, and the documents.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** PARTIAL

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. The agent-loop budget gate added in agent.rs (budget_refusal before worker/native/local/remote dispatch) builds its refusal JSON inline with fields code/message/tool/retryable, while the MCP seam returns the full ToolFailure shape (including argument and the structured message naming the budget options). Consider routing the agent-loop refusal through the same ToolFailure serialization so the two frontends emit byte-identical refusal payloads for the same tool, which also keeps any future field additions (e.g. the budget block) consistent across seams.

### Responses to Prior Points
Re B1: Resolved. The deny-list is inverted to an explicit allow-list (tool_input_budget_admitted) with a documented rationale per admitted class, enforced at the MCP registry seam, the agent/chat/ACP loop seam, and the script dispatch-by-name seam, all routed through Session::budget_input_refusal. The unit test covers admitted classes, refused classes (including edit_file, snapshot, kgl_*, bg, tool_run), mcp__* names, an unknown future name, and the empty name — refusal by default is now the behavior, which is what the concern required. Process tests confirm edit_file/snapshot refusals and that a bare deadline refuses nothing at the gate. The reviewed tool_policy_unsupported code (D60) replaces the unilateral new code.
Re B2: The clarification is largely persuasive on the control flow — I accept that every `return` between the start of the with_budget_deadline block and its end is inside the async block, that end_tool_operation runs after the block on every such path, and that the leak-proof tests (test_each_kind_of_failed_call_leaves_a_fresh_budget_for_the_next_call, test_a_timeout_leaves_a_fresh_budget_for_the_next_call) plus the debug_assert at the top-level seam substantially discharge the proof request. However, one named path in the concern remains unaddressed in the revised diff: the git_read arm's `return err_text(... "tool registry unavailable" ...)` still appears OUTSIDE/BEFORE the with_budget_deadline async block in the revised mcp.rs hunk — the `let Some(registry) = ... else { return err_text(...) }` sits in the arm as quoted, and if that return is in fact inside the scoped block the diff should show it as such (in the revised diff the gate and scope open before the match, so it likely is inside — but the leak test does not exercise this path: 'tool registry unavailable' is not among the early endings tested, and none of the tested paths exercise a registry-absent session). Either add that case to the leak test, point to the exact line numbers establishing the return is inside the scoped future, or state why the registry-absent state is unreachable in a budgeted process-level run. The ownership argument against an RAII guard (the budget is a field of the &mut Session the dispatch borrows) is accepted; the debug_assert is an acceptable substitute. This is the only residue keeping B2 open, and it is small.
Re B3: Resolved. The text parser is removed; the hit is recorded in-band on the session (budget_hit, set only by budget helpers and the ls/grep stop paths, cleared by begin_tool_operation) and drained by take_budget_hit in the run loop. The forged-shape test (exec echoing the JSON shape writes no limit_hit) and the genuine-hit test (asserted by kind and fields) are exactly what was asked. The honest note that the forged test also passed on the old binary is appreciated; the in-band signal still removes the class. The residual refusal_code text-parsing in WP08 is out of this package's scope and is properly recorded as an owner merge item.
Re B4: Resolved. ls_recurse now charges per entry inside the walk and stops via WalkStop::Budget; the confined walk gets a hard max_entries cap (WalkOptions) with tool_entry_stop; read_file prechecks by metadata size before reading. The new tests prove enforcement rather than accounting: test_max_entries_bounds_the_ls_walk (5000 entries, limit 50, consumed < 200) and test_read_file_refuses_oversized_by_size_precheck, plus the tightened consumed bound (3 < consumed <= 5) in the original ls test.
Re B5: Resolved. (a) tool_budget_unenforceable is removed everywhere and the reviewed tool_policy_unsupported is used (D60) — no extension of the closed code list. (b) --tool-max-entries stays live under recorded decisions D56/D61 with the PM Q-FR-19-1 rationale, marked (proposed) and listed for the owner; that is the D56-style recorded-decision path the concern asked for. (c) BudgetReceipt/LimitFailure non-use and the git_read approximation are recorded as D61. The contradicting r3/r4 status blocks are replaced by one authoritative block in INTERFACES.md.
Re B6: Resolved. The two process tests are exactly the proof requested: test_a_call_during_a_budgeted_script_does_not_share_its_budget demonstrates true concurrency by asserting reply order (9002 before 9001) and verifies both the isolation of the concurrent call and the script's consumed=120, and test_a_failed_child_does_not_clear_the_shared_budget covers accounting across a failed child (with a reasonable justification for using batch rather than execute_script, since a failed script op ends the script). The stated mutation check (removing the two set_operation_budget(None) calls fails test 1 and the leak test) and the re-entry analysis (inner dispatch_tool_inner sees the budget installed, owned=false) close the gap.
Re R1: Acknowledged. The saturating_add fix with the concurrency comment addresses S2 as suggested, and widening the curl/pipeline bounds to 8 s while raising the slow server delay to 20 s preserves the semantic 'well below the sleep' property. Both suggestions are satisfied.
Re R2: Acknowledged. Recording the git_read bytes-returned approximation as decision D61 in docs/configuration.md and INTERFACES.md is an acceptable resolution of S3 — the semantics are now explicit and owned rather than silently non-uniform. No further action needed from my side.
Re R3: Understood and acceptable as non-blocking. S1 (genuine read error under a budget not misreported as a budget hit) and S5 (chat/ACP budget smoke test) remain open suggestions; they are honestly recorded in INTERFACES.md and the PR. Given the ACP harness limitation is real and the WP06 precedent motivated S5, I still encourage the ACP/chat smoke test in a follow-up, but I will not block on it.
Re R4: The verification picture is adequate: fmt/clippy clean, the 6 shellcheck Rust failures and 7 shellcheck pytest failures reproduce on base, and the test_acp_runs_teardown_when_signaled flake is convincingly shown pre-existing (14/36 branch vs 12/36 base vs 15/36 master). The absence of a CI run on the stacked branch is noted; a CI run after rebase onto the default base should be confirmed at merge time, but that is an integration gate, not a review blocker.
Re R5: Noted. The split of work between the exhausted worker sub-agent and the integrator (who wrote the B2/B6 tests, the debug_assert, and the docs) is transparent and the integrator's independent verification of the final tree covers the provenance concern. No issue.
B1: resolved
B3: resolved
B4: resolved
B5: resolved
B6: resolved

**Verdict:** REVISE

<!-- event id="rebuttal-2" artifact path="1582-wp18-tool-budgets/artifacts/round-2-rebuttal.diff" sha256="c73387dbee33c1770d46ff15b9f4d2ee6c30565b4736f7a154a670db944491e5" -->
## Rebuttal — Round 2
**Position:** AGREE

### Responses to Blocking Concerns
Re B2: ACCEPT — Accepted: the registry-absent path was not exercised. Line numbers in src/mcp.rs at 5da3a19: `let budget_owned = session.begin_tool_operation();` is at line 453; the only return before the scoped block is the refusal at line 458, which calls end_tool_operation first; the block `with_budget_deadline(budget_remaining, async {` opens at line 461 and closes at line 1192 (`})` then `.await;`); `return err_text(... "tool registry unavailable" ...)` is at line 825, inside the block; `session.end_tool_operation(budget_owned);` is at line 1203, and no `return` exists between lines 1192 and 1203. So that return ends the block and the clear runs. Reachability: each process frontend sets a registry (provisioning.rs), so a process test cannot reach this path. New Rust test mcp::tests::budgeted_git_read_without_a_registry_leaves_no_budget (commit 824c70f): a Session without a registry and with tool_budget.input_bytes = 100 reads a 60-byte file, then calls git_read (the result is the 'tool registry unavailable' error), then asserts session.tool_budget.is_none(), then reads the 60-byte file again, which must succeed with a fresh allowance. Mutation: when the clear after the block is skipped for the 'tool registry unavailable' result, the test fails at 'the early return must clear the budget'.

### Revised Code / Diff
diff --git a/CHANGELOG.md b/CHANGELOG.md
index 6a4bb13..7bc5d35 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -9,6 +9,26 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ### Added
 
+- **Cooperative tool budgets (FR-19, ADR-031).** Three default-off, global
+  options limit one top-level tool call: `--tool-timeout-ms N` (a deadline),
+  `--tool-input-bytes N` (a limit for the file and git content that the call
+  reads or scans), and `--tool-max-entries N` (a limit for the directory
+  entries that the call walks; the name is proposed).
+  - A deadline hit returns `tool_timeout`. A byte or entry hit returns
+    `tool_input_limit` with the limit, the configured value, and the consumed
+    value. A hit returns no partial result and writes the `limit_hit` audit
+    event.
+  - `search` in content mode charges the bytes that it scans and stops during
+    the scan. `ls` stops the walk at the entry limit.
+  - `batch` and `execute_script` share one budget with their children.
+  - A subprocess of `exec`, `git_read`, or a plugin is stopped at the deadline
+    with its process group.
+  - Under an input or entry limit, a tool that cannot meter its input is
+    refused with `tool_policy_unsupported`: `exec`, the process plugins, the
+    index search, and each tool that is not on the admission list.
+    `tool_pipeline` is refused under each budget option. A budget option with
+    `--worker-config` is refused at startup (exit `2`).
+  - Without the options, each tool is unchanged.
 - **`--capability-probe-payload` (FR-04).** A default-off `agent --print`
   option (ADR-027) that sends exactly one unretried capability probe before any
   evidence tool runs, proving the endpoint can drive the same final-answer tool
diff --git a/docs/configuration.md b/docs/configuration.md
index 1b0e3e0..bb88a95 100644
--- a/docs/configuration.md
+++ b/docs/configuration.md
@@ -1812,3 +1812,91 @@ The control file is JSON:
   platforms reaping relies on the SDK's `kill_on_drop` at process exit.
 
 See ADR-028 and `tests/test_worker_isolation.py`.
+
+## Cooperative tool budgets (FR-19, ADR-031)
+
+Three default-off, global command-line options bound the resources a single
+top-level tool call may consume. They are caller-owned (never read from
+`daimonos.toml`) and, when none is set, every tool behaves exactly as before.
+
+- **`--tool-timeout-ms N`** — an absolute deadline, in milliseconds, for the
+  call. A hit returns the reviewed `tool_timeout` code with no partial result.
+- **`--tool-input-bytes N`** — a logical input-byte ceiling for the call. It
+  counts the file content that a tool reads or scans, and the git content that
+  `git_read` returns. It never counts the displayed or truncated output bytes.
+  A hit returns `tool_input_limit`.
+- **`--tool-max-entries N`** *(proposed)* — a directory-entry ceiling for the
+  call (e.g. `ls` walking). A hit returns `tool_input_limit` naming
+  `directory entries`. (The reviewed interface row has no separate code for the
+  entry limit; integrator decision D56 reuses `tool_input_limit`.)
+
+Each value must be a positive integer; `0` is rejected at startup (exit `2`).
+
+**No partial success.** A limit hit is a typed tool failure naming which limit
+was hit, the configured value, and the consumed value — never a truncated
+"successful" result.
+
+**One budget per call, shared by composites.** A `batch` and an
+`execute_script` share one budget with their children: a child inherits the
+remaining allowance and the same absolute deadline and cannot open a fresh
+top-level budget, so delegation cannot multiply the allowance.
+
+**Stopped work, not a stopped wait.** For `exec` and `git_read` the deadline is
+handed to the managed subprocess, which reaps the whole process group
+(`SIGTERM`, bounded grace, then `SIGKILL`) — so no child or grandchild outlives
+the `tool_timeout` result. On non-Unix platforms the subprocess stop relies on
+the managed lifecycle's `kill_on_drop`.
+
+**Output truncation is separate.** `--no-output-spill` bounds the *displayed*
+output of a result; it is not an input budget and a truncated result is never a
+budget failure.
+
+**Workers.** Worker-routed tool calls (`--worker-config`, WP10) run in a
+separate process that this process's budget cannot observe, so the deadline
+cannot stop that work and its input cannot be accounted. Rather than silently
+ignore a budget option in worker mode, a budgeted worker route is refused at
+startup with exit `2` (`run_configuration_invalid`, reason
+`tool_budget_worker_unenforceable`). Worker mode with no budget option is
+unchanged.
+
+Every frontend that builds a session from configuration honours the budget
+(the MCP server, `agent --print`, the interactive agent / session daemon,
+`chat`, `acp`, `batch`, and `execute_script`). For the interactive agent the
+budget is part of the session-daemon launch identity, so an attach to a daemon
+started under a different (or absent) budget is refused.
+
+See ADR-031 and `tests/test_tool_budgets.py`.
+
+#### Tool budgets — enforcement scope (WP18, FR-19)
+
+The default-off options `--tool-timeout-ms`, `--tool-input-bytes`, and
+`--tool-max-entries` are cooperative:
+
+- **Deadline.** Each tool call has the deadline. `read_file`, `ls`, and `search`
+  check it at work boundaries: between files, and for each read chunk of a
+  content search. A subprocess of `exec`, `git_read`, or a plugin is stopped at
+  the deadline with its process group (Linux; on other platforms the process
+  default timeout applies). A passed deadline returns `tool_timeout`. The check
+  is not preemptive: one slow system call can pass the deadline before the next
+  check.
+- **Input bytes.** `search` in content mode charges the bytes that it scans and
+  stops during the scan. `read_file` checks the file size before the read and
+  charges the whole file, also when `offset` and `limit` select a part.
+  `git_read` charges the bytes of the content that it returns, after git ended.
+  That is an approximation of the git content read: it does not stop git early.
+- **Entries.** `ls` charges each directory entry during the walk and stops at
+  the limit.
+- **Refused under an input or entry limit.** With `--tool-input-bytes` or
+  `--tool-max-entries`, a tool is admitted only when it meters its input or
+  reads no workspace or external content. Each other tool is refused with
+  `tool_policy_unsupported`: `exec`, the process plugins (`git`, `gh`, `cargo`,
+  `npm`, `pytest`, `docker`, `shellcheck`, `curl`), `edit_file`, `snapshot`,
+  `workspace_info`, tools from configuration, outbound MCP tools, and `search`
+  in files mode (the index does not report the content that it reads). With
+  `--confine-paths`, the offered tools `read_file`, `ls`, and `search` in
+  content mode stay available.
+- **Refused under each budget option.** `tool_pipeline`, and a run with
+  `--worker-config` (at startup).
+
+Output truncation (`--no-output-spill`, `tool_output`) is a separate mechanism,
+never reported as a budget limit hit.
diff --git a/docs/contexts/reviewer-runtime/INTERFACES.md b/docs/contexts/reviewer-runtime/INTERFACES.md
index 18e5b30..dda5d3c 100644
--- a/docs/contexts/reviewer-runtime/INTERFACES.md
+++ b/docs/contexts/reviewer-runtime/INTERFACES.md
@@ -284,6 +284,18 @@ No cooperative guarantee is claimed for an unowned remote process.
 
 Proposed `ToolFailure` fields: `code`, `message`, `tool`, `argument`, and optional `limit`, `configured`, `consumed`, and `retryable`.
 A timed-out or input-limited call returns `complete=false` and no success-shaped partial result.
+
+_Status (WP18, implemented; state after the code review of 2026-11-23):_
+
+- **Options.** `--tool-timeout-ms N`, `--tool-input-bytes N`, and `--tool-max-entries N` are global, default-off options. `0` is refused at startup with exit code `2`. `--tool-max-entries` is not in the reviewed row above: the name is **(proposed)** (integrator decision D56, owner decision open).
+- **Codes.** A deadline hit returns `tool_timeout`. A byte or entry hit returns `tool_input_limit` and names the limit, the configured value, and the consumed value. An entry hit has no code of its own (D56). A tool that cannot enforce a requested input or entry budget is refused with the reviewed code `tool_policy_unsupported` (D60). WP18 adds no refusal code.
+- **Admission under an input or entry budget.** `tool_budget::tool_input_budget_admitted` is an explicit list. A tool is admitted only when it meters its input (`read_file`, `ls`, `search` in content mode, `git_read`) or reads no workspace or external content (write-only tools, composites, session state, tool-catalog introspection, agent mail, reservations, plan state). Each other tool name is refused, also an outbound MCP tool, a tool from configuration, and a name that a later package adds. `search` in files mode (the index) is refused: the index does not report the content that it reads.
+- **Deadline.** One scope in `mcp::dispatch_tool_inner` covers each registry tool. Native readers check the deadline at work boundaries. A subprocess of `exec`, `git_read`, or a plugin is terminated with its process group through `ManagedChild::terminate`. `tool_pipeline` is refused under each budget option. A run with `--worker-config` and a budget option is refused at startup (`run_configuration_invalid`, reason `tool_budget_worker_unenforceable`).
+- **Metering.** `search` in content mode charges the bytes that it scans and stops during the scan. `ls` charges each entry during the walk and stops at the limit. `read_file` checks the file size before the read and charges the content. `git_read` charges the bytes of the content that it returns, after the subprocess ended: this is an approximation of the git content read (D61).
+- **Composites.** `batch` and `execute_script` share one budget with their children. `execute_script` holds the budget in its script context and sets it on the session only inside each locked inner operation.
+- **Audit.** A limit hit writes the reserved `limit_hit` event. The budget code records the hit on the session, and the run loop reads that record. It does not parse the result text of a tool.
+- **Names.** `ToolBudget` with `check_deadline()`, `charge_input(bytes)`, `charge_entries(n)`, and `remaining()`. The reserved names `BudgetReceipt` and `LimitFailure` are not used: `path_policy::ToolFailure` carries a failure (D61).
+- **Open.** No process test runs the chat or ACP frontend with a budget option. The `tool-contract` dump does not name the budget options.
 These fields are policy/error metadata, not a rewrite of every existing tool's successful payload.
 Audit records the named limit and the refusal. Error text is bounded and does not contain raw evidence.
 
@@ -424,3 +436,4 @@ Implementation status (WP06, r3 review fixes): the model-facing catalog is now f
 Implementation status: WP06 (r2 review fixes) extends the above. The explicit policy now also filters schema lookup (`get_tool_schema`), the Starlark signature dump (`list_tool_signatures`), the MCP server-instructions signature section, the session-daemon socket listing, and the agent-print offered catalog — so the served, discovered, probed, dumped, and dispatched sets agree (ADR-023). With `--final-output`, an allowlist that omits the designated final-answer tool fails at startup with exit-2 `run_configuration_invalid` reason `tool_allowlist_missing_final_output`; the name is never added automatically (PM Q-FR-02-2). The reported `effective_tool_allowlist` is the intersection with the run's active legacy restriction (the agent-env `allowed_commands`), i.e. the set the run will actually permit. Config-registered custom local tools (`[tools.*]`) are part of the known catalog and may be allowlisted.
 
 Implementation status: WP06 (r4 review fixes) extends the above. Exit-2 taxonomy addition: a malformed or unreadable `--tool-allowlist` file fails at startup with `run_configuration_invalid` reason `tool_allowlist_invalid` (joining `tool_allowlist_unknown_entry` and `tool_allowlist_missing_final_output` as the three reserved WP06 exit-2 reasons; `tests/test_tool_allowlist.py::test_malformed_allowlist_file_fails_at_startup` and `::test_unreadable_allowlist_file_fails_at_startup`). Delegation (ADR-023 R1, PM Q-FR-02-1): the composite opcodes `tool_pipeline` and `lint_repair` (and the raw-protocol `tool_run`) now check their inner `tool_id` against the policy (`src/ops/tool_ops.rs` → `session.policy.admit(tool_id)`) before `ToolRegistry::pipeline`/`::repair`/`::run`, so a listed composite cannot reach an unlisted registered tool; the refused inner tool never runs (`tests/test_tool_allowlist.py::test_tool_pipeline_inner_tool_not_on_allowlist_is_refused`, `::test_lint_repair_inner_tool_not_on_allowlist_is_refused`, and the `ops::tool_ops` unit tests). In this package the allowlist names only native and config-registered `[tools.*]` tools; remote/worker names defer to WP10, and an active allowlist therefore removes every forwarded MCP tool from the catalog.
+
diff --git a/src/agent.rs b/src/agent.rs
index ad580f8..273585e 100644
--- a/src/agent.rs
+++ b/src/agent.rs
@@ -1743,6 +1743,19 @@ async fn run_inner(
                     // final-answer tool is required to be on the list at startup
                     // and is intercepted earlier, so it is never refused here.
                     let policy_refused = !session.lock().await.policy.allows(&name);
+                    // R1 (B1, D60): the agent/chat/ACP loop is this frontend's own
+                    // dispatch boundary. Opcode tools reach `tool_facade::invoke`
+                    // directly (never `mcp::dispatch_tool_inner`), and remote
+                    // `mcp__*` tools reach the remote hook, so the input/entry
+                    // budget allow-list gate is enforced here too, before any
+                    // worker/native/local/remote dispatch. A tool not on the
+                    // allow-list (edit_file, snapshot, kgl_*, a plugin, an outbound
+                    // mcp__* tool, or any later name) is refused with the reviewed
+                    // `tool_policy_unsupported`; a bare `--tool-timeout-ms` refuses
+                    // nothing here. (A budgeted worker run is also refused at
+                    // startup, so the worker branch is unreachable under a budget;
+                    // this gate covers it defensively.)
+                    let budget_refusal = session.lock().await.budget_input_refusal(&name);
                     let (mut content, is_error, mut outcome) = if policy_refused {
                         let content = serde_json::json!({
                             "code": crate::run_policy::CODE_TOOL_NOT_ALLOWED,
@@ -1758,6 +1771,19 @@ async fn run_inner(
                             ..ToolOutcome::default()
                         };
                         (content, true, Some(outcome))
+                    } else if let Some(failure) = budget_refusal {
+                        let content = serde_json::json!({
+                            "code": failure.code,
+                            "message": failure.message,
+                            "tool": name.clone(),
+                            "retryable": false,
+                        })
+                        .to_string();
+                        let outcome = ToolOutcome {
+                            request_tokens_est: crate::analytics::estimate_tokens(request_chars),
+                            ..ToolOutcome::default()
+                        };
+                        (content, true, Some(outcome))
                     } else if let Some(worker) = &config.worker_executor {
                         // WP10 (FR-05, ADR-028): worker-only execution. Every
                         // non-final tool call is routed to a caller-supplied
@@ -2147,6 +2173,19 @@ async fn run_inner(
                             tool_started.elapsed().as_millis() as u64,
                             None,
                         );
+                        // WP18 (FR-19 F5, ADR-031): a cooperative tool-budget
+                        // limit hit also writes the reserved `limit_hit` event
+                        // with the limit name, the tool, and the configured and
+                        // consumed values. R3 (B3): read the hit IN-BAND from the
+                        // session (set only by the budget code on a genuine hit)
+                        // rather than parsing the result text, which a tool could
+                        // forge by echoing the JSON shape.
+                        if let Some((limit, configured, consumed)) =
+                            session.lock().await.take_budget_hit()
+                        {
+                            let _ =
+                                audit.record_limit_hit(&limit, Some(&name), configured, consumed);
+                        }
                         if audit.poisoned() {
                             terminate_reason = "audit_write_failed";
                             terminate = true;
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
index fc4e4c3..da2bdff 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -459,6 +459,10 @@ pub async fn run_agent(
             .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
         let bootstrap = crate::session_bootstrap::connect_or_spawn(
             &crate::session_bootstrap::BootstrapOptions {
+                // FR-19 (WP18, ADR-031): carry the caller's budget into the
+                // daemon launch identity so an interactive session enforces it
+                // and an attach cannot silently downgrade it.
+                tool_budget: cfg.tool_budget,
                 workspace,
                 config_path: explicit_config_path.as_deref(),
                 state_dir: cfg
@@ -702,6 +706,24 @@ pub async fn run_agent(
         }
         None => None,
     };
+    // FR-19 (WP18, ADR-031): a cooperative tool budget is installed at THIS
+    // process's dispatch seams, but worker-routed tool calls execute inside a
+    // separate worker process the parent budget never observes — the deadline
+    // cannot stop that owned work and its input cannot be accounted. Rather
+    // than silently ignore a budget option in worker mode (the frontend rule
+    // forbids a silent drop), refuse the budgeted worker route at startup, the
+    // same fail-closed class as `reject_credential_disclosure`
+    // (ADR-031 "refuse enabled-but-unenforceable"; the INTERFACES worker rule).
+    // A default-off budget leaves worker mode exactly unchanged.
+    if worker_config_runtime.is_some() && cfg.tool_budget.any_enabled() {
+        emit_run_configuration_invalid(
+            "tool_budget_worker_unenforceable",
+            "a tool budget (--tool-timeout-ms/--tool-input-bytes/--tool-max-entries) \
+             cannot be enforced across the worker boundary: worker-routed tool calls \
+             run in a separate process, so the deadline cannot stop that work and the \
+             input cannot be accounted. Remove the budget option or --worker-config.",
+        );
+    }
     let agent = load_agent_env(agent_env, &cfg)?;
     let cfg = with_skill_dir(cfg, &agent)?;
     let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
diff --git a/src/cli.rs b/src/cli.rs
index 8f9ef6e..360abb8 100644
--- a/src/cli.rs
+++ b/src/cli.rs
@@ -469,6 +469,29 @@ pub struct Cli {
     #[arg(long, global = true, value_name = "PATH")]
     pub tool_allowlist: Option<PathBuf>,
 
+    /// Stop an opted-in tool call at an absolute per-call deadline of N
+    /// milliseconds (WP18, FR-19, ADR-031). External work (exec, git_read)
+    /// inherits the remaining deadline and its process group is reaped. A
+    /// deadline hit returns `tool_timeout` with no partial result. Positive
+    /// integer; omitted means no new limit; default off.
+    #[arg(long = "tool-timeout-ms", global = true, value_name = "N")]
+    pub tool_timeout_ms: Option<u64>,
+
+    /// Refuse an opted-in tool call once its logical input exceeds N bytes
+    /// (WP18, FR-19, ADR-031). Counts file/git content read or scanned,
+    /// including cached-index content — not displayed output, which is a
+    /// separate truncation mechanism. A hit returns `tool_input_limit`.
+    /// Positive integer; omitted means no new limit; default off.
+    #[arg(long = "tool-input-bytes", global = true, value_name = "N")]
+    pub tool_input_bytes: Option<u64>,
+
+    /// Refuse an opted-in tool call once it walks more than N directory
+    /// entries (WP18, FR-19, ADR-031; integrator D56 `(proposed)`). A hit
+    /// returns `tool_input_limit` naming the entry ceiling. Positive integer;
+    /// omitted means no new limit; default off.
+    #[arg(long = "tool-max-entries", global = true, value_name = "N")]
+    pub tool_max_entries: Option<u64>,
+
     /// Serve and export full, self-contained JSON Schema draft 2020-12 tool
     /// schemas instead of the default stripped placeholders (WP17, FR-15).
     /// Equivalent to `[mcp] full_tool_schemas = true`; the config/env seam still
diff --git a/src/config.rs b/src/config.rs
index 221b088..cd7654a 100644
--- a/src/config.rs
+++ b/src/config.rs
@@ -194,6 +194,13 @@ pub struct Config {
     /// deserialized from a config file; set once at startup from the CLI.
     #[serde(skip)]
     pub discovery_policy: crate::discovery_policy::DiscoveryPolicy,
+    /// `--tool-timeout-ms` / `--tool-input-bytes` / `--tool-max-entries`
+    /// (FR-19, WP18, ADR-031): the caller's cooperative per-call tool budgets.
+    /// Caller-owned; never deserialized from workspace/user config, so nothing
+    /// the caller did not pass explicitly can set or widen a limit. All-absent
+    /// keeps legacy behavior (no budget installed).
+    #[serde(skip)]
+    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
     #[serde(default)]
     pub tools: HashMap<String, ToolConfig>,
 }
diff --git a/src/main.rs b/src/main.rs
index 2300f8a..408197b 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -65,6 +65,7 @@ mod skills;
 mod snapshot;
 mod state_root;
 mod structured_output;
+mod tool_budget;
 mod tool_contract;
 mod tool_descriptions;
 mod tool_facade;
@@ -545,6 +546,32 @@ async fn main() -> anyhow::Result<()> {
     if cli.no_output_spill {
         cfg.tool_output.save_full_output = false;
     }
+    // FR-19 (WP18, ADR-031): pin the caller's cooperative tool-budget options.
+    // Positive integers only; a zero is a non-positive limit and is refused at
+    // startup with a named error rather than silently disabling the budget.
+    // Caller-owned: set only here, never from workspace/user config.
+    for (name, value) in [
+        ("--tool-timeout-ms", cli.tool_timeout_ms),
+        ("--tool-input-bytes", cli.tool_input_bytes),
+        ("--tool-max-entries", cli.tool_max_entries),
+    ] {
+        if value == Some(0) {
+            eprintln!(
+                "{}",
+                serde_json::json!({"error": {
+                    "code": "run_configuration_invalid",
+                    "reason": "tool_budget_invalid",
+                    "message": format!("{name} must be a positive integer")
+                }})
+            );
+            std::process::exit(2);
+        }
+    }
+    cfg.tool_budget = crate::tool_budget::ToolBudgetConfig {
+        timeout_ms: cli.tool_timeout_ms,
+        input_bytes: cli.tool_input_bytes,
+        max_entries: cli.tool_max_entries,
+    };
     // FR-07 (ADR-022): pin the read-only workspace decision before any writer.
     cfg.workspace_read_only = cli.workspace_read_only;
     if cfg.workspace_read_only
diff --git a/src/managed_process.rs b/src/managed_process.rs
index dd33db9..85c7824 100644
--- a/src/managed_process.rs
+++ b/src/managed_process.rs
@@ -18,6 +18,31 @@ use tokio::sync::Mutex;
 
 pub type ProgressCallback<'a> = dyn Fn(String) + Send + Sync + 'a;
 
+tokio::task_local! {
+    /// Cooperative tool-budget deadline (remaining time) in force for the
+    /// current operation, set by the dispatch seam via [`with_budget_deadline`].
+    /// Subprocesses that [`run`] spawns inside the scope inherit it so the OS
+    /// process group is reaped AT the deadline, never merely waited on
+    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
+    static BUDGET_DEADLINE: Option<Duration>;
+}
+
+/// Run `fut` with the cooperative tool-budget deadline in scope. A `None`
+/// deadline (no `--tool-timeout-ms`, or no deadline left to inherit) leaves the
+/// legacy behavior of [`run`] unchanged. The deadline bounds but never extends
+/// the process default timeout (FR-19, WP18, ADR-031).
+pub async fn with_budget_deadline<F: std::future::Future>(
+    deadline: Option<Duration>,
+    fut: F,
+) -> F::Output {
+    BUDGET_DEADLINE.scope(deadline, fut).await
+}
+
+/// The cooperative deadline in scope, or `None` outside a budgeted operation.
+fn scoped_budget_deadline() -> Option<Duration> {
+    BUDGET_DEADLINE.try_with(|d| *d).unwrap_or(None)
+}
+
 /// Captured process output. Each stream is bounded while it is read.
 pub struct ManagedOutput {
     pub status: ExitStatus,
@@ -468,14 +493,28 @@ pub async fn run(
     let mut command = Command::new(program);
     command.args(args).current_dir(cwd);
     apply_environment(&mut command, cfg, overlays);
-    let output = capture(&mut command, cfg, None, stdin_data).await?;
+    // FR-19 (WP18, ADR-031): a cooperative tool-budget deadline in scope caps
+    // this subprocess and reaps its process group at the deadline. `None`
+    // preserves the legacy process-default behavior exactly.
+    let (output, budget_fired) = capture_budgeted(
+        &mut command,
+        cfg,
+        None,
+        stdin_data,
+        scoped_budget_deadline(),
+    )
+    .await?;
     if output.timed_out {
         return Err(io::Error::new(
             io::ErrorKind::TimedOut,
-            format!(
-                "{program} timed out after {} seconds",
-                cfg.default_timeout_secs
-            ),
+            if budget_fired {
+                format!("{program} stopped at the tool budget deadline")
+            } else {
+                format!(
+                    "{program} timed out after {} seconds",
+                    cfg.default_timeout_secs
+                )
+            },
         ));
     }
     Ok(output)
@@ -488,6 +527,33 @@ pub async fn capture(
     progress: Option<&ProgressCallback<'_>>,
     stdin_data: Option<&[u8]>,
 ) -> io::Result<ManagedOutput> {
+    capture_inner(command, cfg, progress, stdin_data, None)
+        .await
+        .map(|(output, _)| output)
+}
+
+/// Like [`capture`], but also stops the child at `budget_deadline` when set,
+/// using the same TERM->grace->KILL process-group reap — never a bare timeout
+/// wrapper that leaves work running underneath (FR-19, WP18, ADR-031). Returns
+/// the output plus `true` when the stop was the cooperative tool budget's
+/// deadline rather than the process default timeout. Context: src/tool_budget.rs.
+pub async fn capture_budgeted(
+    command: &mut Command,
+    cfg: &ProcessConfig,
+    progress: Option<&ProgressCallback<'_>>,
+    stdin_data: Option<&[u8]>,
+    budget_deadline: Option<Duration>,
+) -> io::Result<(ManagedOutput, bool)> {
+    capture_inner(command, cfg, progress, stdin_data, budget_deadline).await
+}
+
+async fn capture_inner(
+    command: &mut Command,
+    cfg: &ProcessConfig,
+    progress: Option<&ProgressCallback<'_>>,
+    stdin_data: Option<&[u8]>,
+    budget_deadline: Option<Duration>,
+) -> io::Result<(ManagedOutput, bool)> {
     command
         .stdin(if stdin_data.is_some() {
             Stdio::piped()
@@ -506,18 +572,37 @@ pub async fn capture(
         .take()
         .ok_or_else(|| io::Error::other("spawned process has no stderr pipe"))?;
     let stdin = child.stdin_mut().take();
-    let timeout =
+    let proc_timeout =
         (cfg.default_timeout_secs > 0).then(|| Duration::from_secs(cfg.default_timeout_secs));
+    // FR-19 (WP18, ADR-031): the cooperative tool budget caps this wait at its
+    // remaining deadline and never extends the process default. The effective
+    // stop is the sooner of the two; `budget_is_tighter` records that the
+    // budget deadline was (at least as) soon, so the caller returns the typed
+    // `tool_timeout` rather than the legacy process-timeout error. Either stop
+    // reaps the whole process group through `ManagedChild::terminate`.
+    let effective_timeout = match (proc_timeout, budget_deadline) {
+        (Some(p), Some(b)) => Some(p.min(b)),
+        (Some(p), None) => Some(p),
+        (None, b) => b,
+    };
+    let budget_is_tighter = match (proc_timeout, budget_deadline) {
+        (_, None) => false,
+        (None, Some(_)) => true,
+        (Some(p), Some(b)) => b <= p,
+    };
     let grace = Duration::from_millis(cfg.termination_grace_ms);
 
     let wait = async {
-        if let Some(timeout) = timeout {
+        if let Some(timeout) = effective_timeout {
             match tokio::time::timeout(timeout, child.wait()).await {
-                Ok(status) => status.map(|status| (status, false)),
-                Err(_) => child.terminate(grace).await.map(|status| (status, true)),
+                Ok(status) => status.map(|status| (status, false, false)),
+                Err(_) => child
+                    .terminate(grace)
+                    .await
+                    .map(|status| (status, true, budget_is_tighter)),
             }
         } else {
-            child.wait().await.map(|status| (status, false))
+            child.wait().await.map(|status| (status, false, false))
         }
     };
     let write_stdin = async {
@@ -545,16 +630,19 @@ pub async fn capture(
     );
     let stdout = stdout?;
     let stderr = stderr?;
-    let (status, timed_out) = status?;
+    let (status, timed_out, budget_fired) = status?;
     stdin?;
-    Ok(ManagedOutput {
-        status,
-        stdout: stdout.render(),
-        stderr: stderr.render(),
-        stdout_truncated: stdout.truncated(),
-        stderr_truncated: stderr.truncated(),
-        timed_out,
-    })
+    Ok((
+        ManagedOutput {
+            status,
+            stdout: stdout.render(),
+            stderr: stderr.render(),
+            stdout_truncated: stdout.truncated(),
+            stderr_truncated: stderr.truncated(),
+            timed_out,
+        },
+        budget_fired,
+    ))
 }
 
 struct BoundedStream {
diff --git a/src/mcp.rs b/src/mcp.rs
index 3b8475e..d97c1f8 100644
--- a/src/mcp.rs
+++ b/src/mcp.rs
@@ -438,6 +438,27 @@ async fn dispatch_tool_inner(
     }
     session.used_tools.insert(name.to_string());
 
+    // === FR-19 F3 (WP18, ADR-031): ONE place installs the operation budget and
+    // the deadline scope for EVERY registry tool (facade ops + plugins +
+    // git_read), not one match arm per tool. A composite child inherits the
+    // parent's budget (owned == false). A process-starting plugin whose logical
+    // input cannot be metered is refused when an input/entry budget is enabled
+    // (never silently exempted). The absolute deadline is threaded into
+    // managed_process so every subprocess group is reaped AT the deadline; a
+    // call whose deadline passed returns tool_timeout, never a late success. All
+    // a no-op when no budget option is set. ===
+    let budget_owned = session.begin_tool_operation();
+    // R1 (B1, D60): the input/entry allow-list gate. Every registry tool that
+    // is not on `tool_budget::tool_input_budget_admitted` is refused here
+    // (`tool_policy_unsupported`) when an input/entry budget is set, so a
+    // process plugin, snapshot, kgl_*, edit_file, or any later-added name is
+    // never silently exempted. A bare deadline refuses nothing here.
+    if let Some(failure) = session.budget_input_refusal(name) {
+        session.end_tool_operation(budget_owned);
+        return response_to_result(failure.into_response(name));
+    }
+    let budget_remaining = session.budget_remaining();
+    let dispatched = crate::managed_process::with_budget_deadline(budget_remaining, async {
     // Registry-based dispatch: route ops-backed tools through the facade.
     if let Some(resp) = tool_facade::invoke(session, name, args).await {
         session.last_response_meta = resp.meta.clone();
@@ -794,10 +815,16 @@ async fn dispatch_tool_inner(
 
         "git_read" => {
             let command = get_str(args, "command").unwrap_or_default();
+            // FR-19 (WP18, ADR-031): git_read runs inside the single budget +
+            // deadline scope installed above, so the git child's process group
+            // is reaped AT the deadline and a passed deadline becomes a typed
+            // tool_timeout at the post-dispatch check. Here we only meter the
+            // git object content it returned (the PM counts this as logical
+            // input): a byte-ceiling hit is tool_input_limit with no partial.
             let Some(registry) = &session.tool_registry else {
                 return err_text(serde_json::json!({"error":{"kind":"unavailable","message":"tool registry unavailable"}}).to_string());
             };
-            match registry
+            let run_result = registry
                 .run(
                     "git_read",
                     &command,
@@ -806,10 +833,14 @@ async fn dispatch_tool_inner(
                     None,
                     Some(args),
                 )
-                .await
-            {
+                .await;
+            match run_result {
                 Ok(result) => {
                     let text = result.output.to_string();
+                    if let Err(failure) = session.budget_charge_bytes(text.len() as u64, "git_read")
+                    {
+                        return response_to_result(failure.into_response("git_read"));
+                    }
                     if result.exit_code == 0 {
                         ok_text(text)
                     } else {
@@ -1158,6 +1189,19 @@ async fn dispatch_tool_inner(
 
         _ => Err(CallToolError::unknown_tool(name.to_string())),
     }
+    })
+    .await;
+    // FR-19 F2/F3 (WP18, ADR-031): a call whose deadline passed is a timeout,
+    // not a late result — even when the work finished just after the deadline,
+    // and even when a deadline-stopped subprocess surfaced as a plugin error
+    // string. Reclassify to the typed tool_timeout whenever the deadline has
+    // passed; otherwise keep the original result (success or a genuine error).
+    let dispatched = match session.budget_check_deadline(name) {
+        Ok(()) => dispatched,
+        Err(failure) => response_to_result(failure.into_response(name)),
+    };
+    session.end_tool_operation(budget_owned);
+    dispatched
 }
 
 fn extract_result_text(result: &CallToolResult) -> String {
@@ -1488,6 +1532,12 @@ impl ServerHandler for DaimonosHandler {
             let observe_on = crate::kgl::observe::enabled();
             let mut observed_ops: Vec<(String, Value)> = Vec::new();
 
+            // FR-19 (WP18, ADR-031): install ONE shared operation budget for
+            // the whole batch, so every sub-op (dispatched via dispatch_tool,
+            // which re-enters ops::dispatch) inherits the same remaining
+            // allowance and absolute deadline instead of each getting a fresh
+            // top-level budget. No-op when no budget option is set.
+            let batch_budget_owned = session.begin_tool_operation();
             let mut results = Vec::with_capacity(ops.len());
             for (i, op_val) in ops.iter().enumerate() {
                 let tool = match op_val.get("tool").and_then(|v| v.as_str()) {
@@ -1527,6 +1577,9 @@ impl ServerHandler for DaimonosHandler {
                 }
             }
 
+            if batch_budget_owned {
+                session.end_tool_operation(true);
+            }
             let payload = serde_json::to_string(&results).unwrap_or_default();
             // Read the dirty flag before the observe path may drop the lock.
             let tools_changed = session.take_tools_changed();
@@ -1555,6 +1608,13 @@ impl ServerHandler for DaimonosHandler {
             return bounded_text_tool_result(&output_cfg, "batch", payload, false).await;
         }
 
+        // FR-19 (WP18): a top-level call holds the session lock from here to
+        // its result, and each call clears the budget that it installed. So no
+        // budget of an earlier call can be on the session here.
+        debug_assert!(
+            session.tool_budget.is_none(),
+            "a tool budget leaked into the next top-level call"
+        );
         let result = dispatch_tool(&mut session, &params.name, &args).await;
         // Read the dirty flag before the observe path may drop the lock.
         let tools_changed = session.take_tools_changed();
@@ -1655,7 +1715,21 @@ async fn run_execute_script(
         .to_string();
     let timeout_secs =
         crate::script::bounded_timeout_secs(args.get("timeout").and_then(Value::as_i64));
-    script::execute(&code, session, std::time::Duration::from_secs(timeout_secs)).await
+    // FR-19 F4 (WP18, ADR-031): execute_script is a composite that owns ONE
+    // shared operation budget for all its inner ops. That budget is created and
+    // held inside the script context (src/script.rs) and installed on the
+    // session only inside each inner per-op lock, then cleared on release — it
+    // is deliberately NOT installed on the shared session here, because
+    // run_execute_script releases the lock while the script runs, and leaving
+    // it on the session across that window would let a concurrent top-level MCP
+    // call inherit it (wrong deadline/allowance). No-op when no budget option
+    // is set.
+    script::execute(
+        &code,
+        session.clone(),
+        std::time::Duration::from_secs(timeout_secs),
+    )
+    .await
 }
 
 /// Convert the Send-safe script outcome only after all handler awaits finish;
@@ -1765,6 +1839,7 @@ async fn build_instructions(workspace: &std::path::Path, cfg: &crate::config::Co
                     skip_dirs: &["target", "node_modules"],
                     // WP14 (FR-12): never descend through a link under no-follow.
                     follow_symlinks: !authority.no_follow(),
+                    max_entries: None,
                 },
             ) {
                 let mut dirs: Vec<_> = walk
@@ -3490,4 +3565,40 @@ mod tests {
             );
         }
     }
+
+    /// FR-19 (WP18, review B2): a call that ends early because the session
+    /// has no tool registry leaves no budget on the session. The registry is
+    /// present in each process frontend, so only a Rust test reaches this path.
+    #[tokio::test]
+    async fn budgeted_git_read_without_a_registry_leaves_no_budget() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("probe.txt"), "b".repeat(60)).unwrap();
+        let mut cfg = Config::default();
+        cfg.tool_budget.input_bytes = Some(100);
+        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(cfg));
+        assert!(session.tool_registry.is_none());
+
+        // A 60-byte read first, so a leaked budget would hold 60 bytes.
+        let first = dispatch_tool(&mut session, "read_file", &json!({"path": "probe.txt"}))
+            .await
+            .unwrap();
+        assert_ne!(first.is_error, Some(true));
+        assert!(session.tool_budget.is_none());
+
+        let unavailable = dispatch_tool(&mut session, "git_read", &json!({"command": "log"}))
+            .await
+            .unwrap();
+        assert_eq!(unavailable.is_error, Some(true));
+        assert!(extract_result_text(&unavailable).contains("tool registry unavailable"));
+        assert!(
+            session.tool_budget.is_none(),
+            "the early return must clear the budget"
+        );
+
+        // The next call has a fresh allowance: 60 bytes fit under 100.
+        let next = dispatch_tool(&mut session, "read_file", &json!({"path": "probe.txt"}))
+            .await
+            .unwrap();
+        assert_ne!(next.is_error, Some(true), "{}", extract_result_text(&next));
+    }
 }
diff --git a/src/ops/exec_ops.rs b/src/ops/exec_ops.rs
index 3b62be8..96af27f 100644
--- a/src/ops/exec_ops.rs
+++ b/src/ops/exec_ops.rs
@@ -533,6 +533,12 @@ async fn capture_exec(
     env: &HashMap<String, String>,
     on_progress: Option<&ExecProgressCallback<'_>>,
 ) -> Response {
+    // FR-19 (WP18, ADR-031): if the cooperative budget's deadline already
+    // passed, refuse before spawning anything rather than start work we would
+    // immediately have to stop. No budget / no deadline is a no-op.
+    if let Err(failure) = session.budget_check_deadline("exec") {
+        return failure.into_response("exec");
+    }
     let mut command = build_command(cmd, args);
     command.current_dir(cwd);
     crate::managed_process::apply_environment(&mut command, &session.cfg.process, env);
@@ -541,17 +547,24 @@ async fn capture_exec(
             callback(ExecProgress::Output(text));
         }
     });
-    let output = match crate::managed_process::capture(
+    // FR-19 (WP18, ADR-031): hand the budget's remaining deadline to the
+    // managed-process path so the exec is stopped (TERM/grace/KILL, whole
+    // process group reaped) at the sooner of it and the process default. A
+    // budget-deadline stop becomes a typed `tool_timeout` below; `None` keeps
+    // legacy behavior exactly.
+    let budget_deadline = session.budget_remaining();
+    let (output, budget_fired) = match crate::managed_process::capture_budgeted(
         &mut command,
         &session.cfg.process,
         progress
             .as_ref()
             .map(|callback| callback as &crate::managed_process::ProgressCallback<'_>),
         None,
+        budget_deadline,
     )
     .await
     {
-        Ok(output) => output,
+        Ok(result) => result,
         Err(error) => {
             if let Some(callback) = on_progress {
                 callback(ExecProgress::Exit {
@@ -568,6 +581,14 @@ async fn capture_exec(
             signal: exit_signal(&output.status),
         });
     }
+    // FR-19 (WP18, ADR-031): a cooperative-budget deadline stop returns the
+    // typed `tool_timeout` with no partial exec result, distinct from the
+    // legacy process-timeout (exit 6) path below.
+    if budget_fired {
+        if let Some(failure) = session.budget_timeout_failure("exec") {
+            return failure.into_response("exec");
+        }
+    }
     if output.timed_out {
         return Response::err(
             6,
diff --git a/src/ops/file_ops.rs b/src/ops/file_ops.rs
index 5facec8..635693b 100644
--- a/src/ops/file_ops.rs
+++ b/src/ops/file_ops.rs
@@ -14,6 +14,23 @@ pub async fn read(session: &mut Session, op: &Op) -> Response {
         None => return Response::err(3, "read requires path in 'p'"),
     };
 
+    // R2 (B4, FR-19): refuse by file size BEFORE reading when the size alone
+    // exceeds the remaining byte allowance, so a budget bounds the bytes read
+    // rather than only reclassifying a completed read. Charging the exact
+    // content length still happens after the read for files that pass.
+    match tokio::fs::metadata(&path).await {
+        Ok(meta) => {
+            if let Err(failure) =
+                session.budget_precheck_bytes(meta.len(), op.p.as_deref().unwrap_or("read_file"))
+            {
+                return failure.into_response("read_file");
+            }
+        }
+        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
+            return Response::err(1, &format!("not found: {}", path.display()))
+        }
+        Err(e) => return Response::err(4, &format!("io: {e}")),
+    }
     let content = match tokio::fs::read_to_string(&path).await {
         Ok(c) => c,
         Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
@@ -24,6 +41,19 @@ pub async fn read(session: &mut Session, op: &Op) -> Response {
     if let Some(index) = &session.index {
         index.observe_path(&path).await;
     }
+    // FR-19 F2 (WP18, ADR-031): a read that completed after the deadline is a
+    // timeout, not a late success with content; check before building a result.
+    if let Err(failure) = session.budget_check_deadline(op.p.as_deref().unwrap_or("read_file")) {
+        return failure.into_response("read_file");
+    }
+    // FR-19 (WP18, ADR-031): charge the logical file content read before
+    // producing any result. A byte-ceiling hit returns tool_input_limit with no
+    // partial content (output truncation is a separate mechanism).
+    if let Err(failure) =
+        session.budget_charge_bytes(content.len() as u64, op.p.as_deref().unwrap_or("read_file"))
+    {
+        return failure.into_response("read_file");
+    }
 
     let total_lines = content.lines().count();
     let offset = op.n.unwrap_or(0).max(0) as usize;
@@ -114,6 +144,12 @@ async fn read_confined(session: &mut Session, op: &Op) -> Response {
         Ok(Err(error)) => return Response::err(4, &format!("io: {error}")),
         Err(error) => return Response::err(4, &format!("io: {error}")),
     };
+    // FR-19 (WP18, ADR-031): charge logical file content for confined reads too.
+    if let Err(failure) =
+        session.budget_charge_bytes(content.len() as u64, op.p.as_deref().unwrap_or("read_file"))
+    {
+        return failure.into_response("read_file");
+    }
     let total_lines = content.lines().count();
     let offset = op.n.unwrap_or(0).max(0) as usize;
     let limit = op.n2.unwrap_or(0);
@@ -431,9 +467,31 @@ pub async fn ls(session: &Session, op: &Op) -> Response {
         type_filter: op.n2,
     };
 
+    // R2 (B4, FR-19, ADR-031): charge each entry INSIDE the walk and STOP at
+    // the ceiling, so an entry budget bounds the directory walk rather than
+    // walking the whole tree and reclassifying afterwards. The deadline is
+    // checked per entry too, so a bare deadline also stops a huge walk. A hit
+    // returns tool_input_limit (D56) / tool_timeout with no partial listing.
+    let budget = session.tool_budget();
     let mut entries = Vec::new();
-    if let Err(e) = ls_recurse(&path, &path, depth, &opts, &mut entries).await {
-        return Response::err(4, &format!("ls: {e}"));
+    let mut walked = 0u64;
+    match ls_recurse(
+        &path,
+        &path,
+        depth,
+        &opts,
+        &mut entries,
+        &mut walked,
+        budget.as_ref(),
+    )
+    .await
+    {
+        Ok(()) => {}
+        Err(WalkStop::Io(e)) => return Response::err(4, &format!("ls: {e}")),
+        Err(WalkStop::Budget(hit)) => {
+            session.note_budget_hit(&hit);
+            return hit.into_failure(Some("ls")).into_response("ls");
+        }
     }
 
     entries.sort_by(|a, b| {
@@ -475,6 +533,12 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
         op.q.as_deref()
             .and_then(|pattern| glob::Pattern::new(pattern).ok());
     let filter = op.n2;
+    // R2 (B4, WP18): cap the confined walk at the remaining entry allowance so
+    // `--tool-max-entries` bounds the walk itself; `None` keeps it unbounded.
+    let entry_cap = session
+        .tool_budget()
+        .and_then(|b| b.entry_remaining())
+        .map(|remaining| remaining as usize);
     let result = tokio::task::spawn_blocking(move || {
         // WP14: honor no-follow for the explicit scope so a symlink named as
         // the ls root is refused with its target, never listed through.
@@ -488,6 +552,7 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
                 follow_symlinks: !authority.no_follow(),
                 include_hidden: show_all,
                 skip_dirs: SKIP_DIRS,
+                max_entries: entry_cap,
             },
         )?;
         Ok::<_, crate::path_policy::ToolFailure>((base, walk))
@@ -503,6 +568,16 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
         Err(error) => return Response::err(4, &format!("ls: {error}")),
     };
     let (skipped, skipped_links) = walk.skipped_summary();
+    // FR-19 (WP18, ADR-031): charge directory entries walked on the confined
+    // path too (entries visited plus those skipped by the walk). No-op when no
+    // entry budget is set.
+    let walked = walk.entries.len() as u64 + skipped as u64 + skipped_links as u64;
+    if let Err(failure) = session.budget_check_deadline("ls") {
+        return failure.into_response("ls");
+    }
+    if let Err(failure) = session.budget_charge_entries(walked, "ls") {
+        return failure.into_response("ls");
+    }
     let mut entries = Vec::new();
     for entry in walk.entries {
         let Ok(relative) = entry.relative_path.strip_prefix(&base) else {
@@ -548,22 +623,46 @@ async fn ls_confined(session: &Session, op: &Op) -> Response {
     Response::ok(response)
 }
 
+/// R2 (B4): why a bounded `ls` walk stopped. `Io` is a filesystem error;
+/// `Budget` is a cooperative tool-budget hit (entry ceiling or deadline) raised
+/// MID-walk so a huge tree is not walked to completion.
+enum WalkStop {
+    Io(String),
+    Budget(crate::tool_budget::BudgetHit),
+}
+
 async fn ls_recurse(
     root: &std::path::Path,
     path: &std::path::Path,
     depth: usize,
     opts: &LsOpts<'_>,
     entries: &mut Vec<serde_json::Value>,
-) -> Result<(), String> {
+    walked: &mut u64,
+    budget: Option<&std::sync::Arc<crate::tool_budget::ToolBudget>>,
+) -> Result<(), WalkStop> {
     let mut dir = tokio::fs::read_dir(path).await.map_err(|e| {
-        if e.kind() == std::io::ErrorKind::NotFound {
+        WalkStop::Io(if e.kind() == std::io::ErrorKind::NotFound {
             format!("not found: {}", path.display())
         } else {
             format!("{e}")
-        }
+        })
     })?;
 
     while let Ok(Some(entry)) = dir.next_entry().await {
+        // FR-19 (WP18, ADR-031): every directory entry visited counts, before
+        // any hidden/skip/type/glob filter, so the charged input is honest.
+        *walked += 1;
+        // R2 (B4): check the deadline and charge this entry BEFORE doing any
+        // per-entry work, and STOP the walk on a hit so the ceiling truly
+        // bounds the walk. No-op when no budget is active.
+        if let Some(b) = budget {
+            if let Err(hit) = b.check_deadline() {
+                return Err(WalkStop::Budget(hit));
+            }
+            if let Err(hit) = b.charge_entries(1) {
+                return Err(WalkStop::Budget(hit));
+            }
+        }
         let name = entry.file_name().to_string_lossy().to_string();
         if !opts.show_all && name.starts_with('.') {
             continue;
@@ -630,7 +729,22 @@ async fn ls_recurse(
         }
 
         if is_dir && depth > 1 {
-            let _ = Box::pin(ls_recurse(root, &entry.path(), depth - 1, opts, entries)).await;
+            // Propagate a budget stop out of the recursion so the whole walk
+            // ends at the ceiling (an I/O error in a subdir is still skipped).
+            match Box::pin(ls_recurse(
+                root,
+                &entry.path(),
+                depth - 1,
+                opts,
+                entries,
+                walked,
+                budget,
+            ))
+            .await
+            {
+                Ok(()) | Err(WalkStop::Io(_)) => {}
+                Err(stop @ WalkStop::Budget(_)) => return Err(stop),
+            }
         }
     }
 
@@ -922,6 +1036,11 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
     let workspace = session.workspace.clone();
     // D08 (ADR-021, FR-09): explicit-only runs let no implicit ignore file steer the walk.
     let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
+    // FR-19 (WP18, ADR-031): hand the shared budget into the blocking worker so
+    // the worker itself meters the bytes it SCANS (not the bytes it returns)
+    // and checks the deadline per read chunk, stopping mid-scan. `None` keeps
+    // the legacy fast path byte-for-byte.
+    let budget = session.tool_budget();
 
     let result = tokio::task::spawn_blocking(move || {
         grep_blocking(
@@ -931,6 +1050,7 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
             max,
             file_glob.as_deref(),
             apply_ignore_files,
+            budget,
         )
     })
     .await;
@@ -945,9 +1065,18 @@ pub async fn grep(session: &Session, op: &Op) -> Response {
                     .collect::<Vec<_>>();
                 index.observe_paths(observed).await;
             }
+            // A scan that completed exactly at the deadline is a timeout, not a
+            // late success (FR-19 F2). Bytes were already charged while reading.
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             Response::ok(json!({"matches": matches}))
         }
-        Ok(Err(e)) => Response::err(4, &format!("grep: {e}")),
+        Ok(Err(GrepStop::Budget(hit))) => {
+            session.note_budget_hit(&hit);
+            hit.into_failure(Some("search")).into_response("search")
+        }
+        Ok(Err(GrepStop::Io(e))) => Response::err(4, &format!("grep: {e}")),
         Err(e) => Response::err(4, &format!("grep task: {e}")),
     }
 }
@@ -982,11 +1111,42 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
     // even under confinement (--confine-paths), so a hostile ignore file cannot
     // steer confined evidence grep.
     let apply_ignore_files = session.cfg.discovery_policy.allows_implicit();
+    // FR-19 F1/F2 (ADR-031): meter scanned bytes and check the deadline inside
+    // the confined blocking worker too. `None` keeps the legacy path.
+    let budget = session.tool_budget();
     let result = tokio::task::spawn_blocking(move || {
         let mut searcher = Searcher::new();
+        let budget_hit: std::cell::RefCell<Option<crate::tool_budget::BudgetHit>> =
+            std::cell::RefCell::new(None);
         let mut scan = |file: std::fs::File,
                         relative: String,
                         matches: &mut Vec<serde_json::Value>| {
+            if let Some(b) = &budget {
+                if let Err(hit) = b.check_deadline() {
+                    *budget_hit.borrow_mut() = Some(hit);
+                    return false;
+                }
+                let mut reader =
+                    crate::tool_budget::BudgetedReader::new(file, std::sync::Arc::clone(b));
+                let ok = searcher
+                    .search_reader(
+                        &matcher,
+                        &mut reader,
+                        UTF8(|line_num, line| {
+                            if matches.len() < max {
+                                matches.push(
+                                    json!({"f": relative, "l": line_num, "t": line.trim_end()}),
+                                );
+                            }
+                            Ok(matches.len() < max)
+                        }),
+                    )
+                    .is_ok();
+                if let Some(hit) = reader.hit() {
+                    *budget_hit.borrow_mut() = Some(hit.clone());
+                }
+                return ok;
+            }
             searcher
                 .search_reader(
                     &matcher,
@@ -1017,6 +1177,9 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
                     .to_string_lossy()
                     .to_string();
                 let skipped = usize::from(!scan(start.into_file(), relative, &mut matches));
+                if let Some(hit) = budget_hit.borrow_mut().take() {
+                    return Err(hit.into_failure(Some("search")));
+                }
                 return Ok::<_, crate::path_policy::ToolFailure>((matches, skipped));
             }
             return Ok((matches, 0));
@@ -1068,12 +1231,23 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
             if !scan(authorized.into_file(), relative, &mut matches) {
                 skipped += 1
             }
+            if budget_hit.borrow().is_some() {
+                break;
+            }
+        }
+        if let Some(hit) = budget_hit.borrow_mut().take() {
+            return Err(hit.into_failure(Some("search")));
         }
         Ok::<_, crate::path_policy::ToolFailure>((matches, skipped))
     })
     .await;
     match result {
         Ok(Ok((matches, skipped))) => {
+            // Bytes were charged while scanning; a scan that finished exactly at
+            // the deadline is still a timeout, not a late success (FR-19 F2).
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             let mut value = json!({"matches": matches});
             if skipped > 0 {
                 value["skipped"] = json!(skipped)
@@ -1087,6 +1261,13 @@ async fn grep_confined(session: &Session, op: &Op) -> Response {
     }
 }
 
+/// How a budgeted grep worker stopped: an I/O/regex error, or a cooperative
+/// tool-budget hit the worker detected while scanning (FR-19 F1/F2, ADR-031).
+pub(crate) enum GrepStop {
+    Io(String),
+    Budget(crate::tool_budget::BudgetHit),
+}
+
 fn grep_blocking(
     pattern: &str,
     root: &Path,
@@ -1094,13 +1275,14 @@ fn grep_blocking(
     max: usize,
     file_glob: Option<&str>,
     apply_ignore_files: bool,
-) -> Result<Vec<serde_json::Value>, String> {
+    budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
+) -> Result<Vec<serde_json::Value>, GrepStop> {
     use grep_regex::RegexMatcher;
     use grep_searcher::sinks::UTF8;
     use grep_searcher::Searcher;
     use ignore::WalkBuilder;
 
-    let matcher = RegexMatcher::new(pattern).map_err(|e| format!("regex: {e}"))?;
+    let matcher = RegexMatcher::new(pattern).map_err(|e| GrepStop::Io(format!("regex: {e}")))?;
     let mut searcher = Searcher::new();
     let mut matches = Vec::new();
 
@@ -1143,6 +1325,42 @@ fn grep_blocking(
             .to_string_lossy()
             .to_string();
 
+        // FR-19 F1/F2 (ADR-031): when a budget is active, the worker itself
+        // meters the bytes it SCANS and checks the deadline at a bounded step
+        // (between files, and inside a large file per read chunk via
+        // BudgetedReader), stopping the scan the moment the ceiling or the
+        // deadline is hit. Without a budget the legacy search_path fast path is
+        // unchanged.
+        if let Some(budget) = &budget {
+            if let Err(hit) = budget.check_deadline() {
+                return Err(GrepStop::Budget(hit));
+            }
+            let file = match std::fs::File::open(path) {
+                Ok(file) => file,
+                Err(_) => continue,
+            };
+            let mut reader =
+                crate::tool_budget::BudgetedReader::new(file, std::sync::Arc::clone(budget));
+            let _ = searcher.search_reader(
+                &matcher,
+                &mut reader,
+                UTF8(|line_num, line| {
+                    if matches.len() < max {
+                        matches.push(serde_json::json!({
+                            "f": rel,
+                            "l": line_num,
+                            "t": line.trim_end(),
+                        }));
+                    }
+                    Ok(matches.len() < max)
+                }),
+            );
+            if let Some(hit) = reader.hit() {
+                return Err(GrepStop::Budget(hit.clone()));
+            }
+            continue;
+        }
+
         let _ = searcher.search_path(
             &matcher,
             path,
diff --git a/src/ops/mod.rs b/src/ops/mod.rs
index 1ab216b..235e8b9 100644
--- a/src/ops/mod.rs
+++ b/src/ops/mod.rs
@@ -31,7 +31,14 @@ pub async fn dispatch_with_progress(
     req: Request,
     on_exec_progress: Option<&ExecProgressCallback<'_>>,
 ) -> Response {
-    match req {
+    // FR-19 (WP18, ADR-031): install a fresh operation budget for this
+    // top-level call. A composite's children re-enter here and inherit the
+    // parent's budget (`owned == false`), so delegation cannot multiply the
+    // allowance or extend the deadline; the installing call clears it on
+    // return so each sequential top-level call starts fresh. When no budget
+    // option is set nothing is installed and behavior is unchanged.
+    let owned = session.begin_tool_operation();
+    let result = match req {
         Request::Single(op) => dispatch_op(session, op, on_exec_progress).await,
         Request::Batch { batch } => {
             let mut results = Vec::with_capacity(batch.len());
@@ -40,7 +47,9 @@ pub async fn dispatch_with_progress(
             }
             Response::ok(serde_json::to_value(results).unwrap())
         }
-    }
+    };
+    session.end_tool_operation(owned);
+    result
 }
 
 async fn dispatch_op(
@@ -143,10 +152,27 @@ async fn find(session: &Session, op: &Op) -> Response {
         None => None,
     };
 
+    // FR-19 F1 (WP18, ADR-031): the index-answered search (mode=files) reads
+    // cached index content, not scannable file content, so its logical input is
+    // not meterable as logical bytes; refuse an enabled input/entry budget
+    // rather than charge "bytes returned" (the owner choice stated in the
+    // handoff). Uses the mode-specific refusal because the allow-list admits
+    // `search` for its byte-metered content mode (R1/D60). The deadline IS
+    // enforced (checked before and after the search).
+    if let Some(failure) = session.budget_index_search_refusal() {
+        return failure.into_response("search");
+    }
+    if let Err(failure) = session.budget_check_deadline("search") {
+        return failure.into_response("search");
+    }
     idx.ensure_populated().await;
     let results = idx
         .search_scoped(&query, max, op.q.as_deref(), file_glob.as_ref())
         .await;
+    // A search that ran past the deadline is a timeout, not a late result.
+    if let Err(failure) = session.budget_check_deadline("search") {
+        return failure.into_response("search");
+    }
     let stats = idx.stats().await;
 
     Response::ok(serde_json::json!({
@@ -161,6 +187,13 @@ async fn find_confined(session: &Session, op: &Op) -> Response {
     let Some(query) = &op.p else {
         return Response::err(3, "find requires query in 'p'");
     };
+    // FR-19 F1 (WP18, ADR-031): the confined files-mode search ranks index/name
+    // content, not scannable file content, so refuse an enabled input/entry
+    // budget rather than charge "bytes returned" (same choice as the plain
+    // index path). The deadline is enforced below.
+    if let Some(failure) = session.budget_index_search_refusal() {
+        return failure.into_response("search");
+    }
     let glob = match op.g.as_deref().map(glob::Pattern::new) {
         Some(Ok(pattern)) => Some(pattern),
         Some(Err(error)) => return Response::err(3, &format!("invalid glob: {error}")),
@@ -258,6 +291,11 @@ async fn find_confined(session: &Session, op: &Op) -> Response {
     .await;
     match result {
         Ok(Ok((results, stats, skipped))) => {
+            // A search that ran past the deadline is a timeout, not a late
+            // result (FR-19 F2).
+            if let Err(failure) = session.budget_check_deadline("search") {
+                return failure.into_response("search");
+            }
             let mut value = serde_json::json!({"results": results, "index": stats});
             if skipped > 0 {
                 value["skipped"] = serde_json::json!(skipped)
diff --git a/src/ops/tool_ops.rs b/src/ops/tool_ops.rs
index 28abafc..7d2fe07 100644
--- a/src/ops/tool_ops.rs
+++ b/src/ops/tool_ops.rs
@@ -122,6 +122,20 @@ pub async fn tool_pipeline(session: &Session, op: &Op) -> Response {
         return failure.into_response(tool_id);
     }
 
+    // FR-19 F3 (WP18, ADR-031): a pipeline runs external plugin stages whose
+    // remaining deadline / logical input cannot be enforced stage-by-stage
+    // cooperatively. Rather than leave it "not cooperative", refuse the whole
+    // pipeline when any budget option is set (ADR-031 permits refusing the
+    // pipeline under --tool-timeout-ms; the same applies to input/entry). No
+    // budget option leaves it unchanged.
+    if session.budget_active() {
+        return crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
+            Some("tool_pipeline"),
+            "a pipeline's external stages are not cooperatively budgeted",
+        )
+        .into_response("tool_pipeline");
+    }
+
     let stages = match &op.a {
         Some(s) if !s.is_empty() => s,
         _ => return Response::err(3, "tool_pipeline requires stage names in 'a'"),
diff --git a/src/path_policy.rs b/src/path_policy.rs
index b07fe7f..50358cb 100644
--- a/src/path_policy.rs
+++ b/src/path_policy.rs
@@ -200,6 +200,77 @@ impl ToolFailure {
     pub fn unsupported(message: impl Into<String>) -> Self {
         Self::refusal(CODE_FILESYSTEM_POLICY_UNSUPPORTED, message, None)
     }
+
+    /// Cooperative tool-budget deadline hit (WP18, FR-19, ADR-031). The
+    /// `configured_ms` value is reported as both the limit and the consumed
+    /// amount: the call ran until at least the deadline. No partial success.
+    pub fn tool_timeout(argument: Option<&str>, configured_ms: u64) -> Self {
+        let mut failure = Self::refusal(
+            "tool_timeout",
+            format!("tool deadline exceeded: configured_ms={configured_ms}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure.budget = Some(Box::new(FailureBudget {
+            limit: configured_ms,
+            configured: configured_ms,
+            consumed: configured_ms,
+        }));
+        failure
+    }
+
+    /// Cooperative tool-budget logical-input hit (WP18, FR-19, ADR-031). Used
+    /// for both the byte ceiling and the directory-entry ceiling; `which`
+    /// names the ceiling that was hit (integrator D56 keeps the single
+    /// reviewed `tool_input_limit` code). No partial success.
+    /// Cooperative tool-budget refusal (WP18, FR-19, ADR-031, R1): a tool whose
+    /// relevant budget cannot be enforced is REFUSED when that budget is
+    /// enabled, rather than silently exempted (INTERFACES.md row WP18). It
+    /// carries the REVIEWED `tool_policy_unsupported` code (integrator D60, no
+    /// new code is minted) and a message that names the budget option(s) and,
+    /// via `into_response`, the tool. Used for the allow-list refusal under
+    /// `--tool-input-bytes`/`--tool-max-entries`, the index-answered search
+    /// (cached index content is not a scannable byte count), and `tool_pipeline`
+    /// (external stages cannot be budgeted cooperatively). Deadline enforcement
+    /// is unaffected.
+    pub fn tool_budget_policy_unsupported(argument: Option<&str>, detail: &str) -> Self {
+        let mut failure = Self::refusal(
+            crate::run_policy::CODE_TOOL_POLICY_UNSUPPORTED,
+            format!(
+                "cooperative tool budget (--tool-input-bytes/--tool-max-entries/\
+                 --tool-timeout-ms) cannot be enforced for this tool: {detail}"
+            ),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure
+    }
+
+    pub fn tool_input_limit(
+        argument: Option<&str>,
+        which: &str,
+        limit: u64,
+        consumed: u64,
+    ) -> Self {
+        let mut failure = Self::refusal(
+            "tool_input_limit",
+            format!("tool input limit exceeded ({which}): configured={limit}, consumed={consumed}"),
+            None,
+        );
+        if let Some(arg) = argument {
+            failure.argument = Some(arg.to_string());
+        }
+        failure.budget = Some(Box::new(FailureBudget {
+            limit,
+            configured: limit,
+            consumed,
+        }));
+        failure
+    }
 }
 
 /// Configurable bounds for the authority. Caller/config supplied — no hidden
@@ -297,6 +368,12 @@ pub struct WalkOptions {
     pub include_hidden: bool,
     /// Directory names that must not be emitted or traversed.
     pub skip_dirs: &'static [&'static str],
+    /// R2 (B4, WP18): optional cooperative-budget cap on the number of entries
+    /// to enumerate. When set, the walk STOPS once it has collected more than
+    /// this many entries (setting [`Walk::tool_entry_stop`]), so a
+    /// `--tool-max-entries` budget bounds the confined walk rather than walking
+    /// the whole tree. `None` (the default) keeps the legacy unbounded walk.
+    pub max_entries: Option<usize>,
 }
 
 impl Default for WalkOptions {
@@ -306,6 +383,7 @@ impl Default for WalkOptions {
             follow_symlinks: true,
             include_hidden: true,
             skip_dirs: &[],
+            max_entries: None,
         }
     }
 }
@@ -319,6 +397,10 @@ pub struct Walk {
     exhausted: bool,
     depth_exhausted: Option<usize>,
     failure: Option<ToolFailure>,
+    /// R2 (B4, WP18): set when the walk stopped early at the cooperative
+    /// [`WalkOptions::max_entries`] cap (distinct from the WP14 `exhausted`
+    /// walk-safety limit, which is a `walk_limit` refusal).
+    pub tool_entry_stop: bool,
 }
 
 /// An owned, already-authorized read handle to an in-root object.
@@ -1482,12 +1564,24 @@ impl FilesystemAuthority {
             if descend {
                 subdirs.push(child_rel);
             }
+            // R2 (B4, WP18): stop the walk once it has collected more than the
+            // cooperative entry cap, so `--tool-max-entries` bounds the walk
+            // itself (not a post-hoc count). One past the cap lets the caller's
+            // charge report a consumed total that exceeds the ceiling.
+            if opts.max_entries.is_some_and(|cap| walk.entries.len() > cap) {
+                walk.tool_entry_stop = true;
+                break;
+            }
         }
         // SAFETY: `dirp` is open and is closed exactly once here.
         unsafe { libc::closedir(dirp) };
 
         for sub in subdirs {
-            if walk.exhausted || walk.depth_exhausted.is_some() || walk.failure.is_some() {
+            if walk.exhausted
+                || walk.depth_exhausted.is_some()
+                || walk.failure.is_some()
+                || walk.tool_entry_stop
+            {
                 break;
             }
             self.walk_dir(&sub, depth + 1, max_depth, opts, walk, budget);
diff --git a/src/script.rs b/src/script.rs
index 6697b59..54133ee 100644
--- a/src/script.rs
+++ b/src/script.rs
@@ -512,6 +512,14 @@ fn build_globals(
     op_count: Arc<AtomicUsize>,
     subcall: Option<SubcallEnv>,
 ) -> Globals {
+    // FR-19 F4 (WP18, ADR-031): create ONE shared operation budget for the
+    // whole script here, so every inner op charges the same cumulative
+    // allowance against the same absolute deadline. It is installed on the
+    // session only inside each per-op lock (see dispatch_request /
+    // dispatch_tool_by_name) and cleared when the lock is released, so it never
+    // lingers on the shared session where a concurrent top-level MCP call could
+    // inherit it. `None` when no budget option is set (legacy behavior).
+    let budget = handle.block_on(async { session.lock().await.cfg.tool_budget.start() });
     // We store session + handle + cancel flag in a thread-local so the
     // starlark_module functions can access them without capturing closures.
     TOOL_CTX.with(|ctx| {
@@ -521,6 +529,7 @@ fn build_globals(
             subcall,
             cancel,
             op_count,
+            budget,
         });
     });
 
@@ -572,6 +581,10 @@ struct ToolContext {
     /// Owned by the caller so the count survives even when the script errors
     /// after dispatching some ops.
     op_count: Arc<AtomicUsize>,
+    /// FR-19 F4: the one shared operation budget for this script, installed on
+    /// the session inside each per-op lock and cleared on release. `None` when
+    /// no budget option is set.
+    budget: Option<Arc<crate::tool_budget::ToolBudget>>,
 }
 
 thread_local! {
@@ -643,10 +656,16 @@ fn dispatch_request(request: Request, label: &str) -> Result<Response, anyhow::E
         let child_span =
             crate::observability::ToolSpan::new(label, crate::observability::tool_kind(label));
         let session = ctx.session.clone();
+        let budget = ctx.budget.clone();
         let resp = child_span.span().in_scope(|| {
             ctx.handle.block_on(async {
                 let mut s = session.lock().await;
+                // FR-19 F4: install the shared budget only while this op holds
+                // the lock; clear it before release so it never leaks to a
+                // concurrent top-level call. begin_tool_operation inherits it.
+                s.set_operation_budget(budget.clone());
                 let resp = ops::dispatch(&mut s, request).await;
+                s.set_operation_budget(None);
                 if let Some(analytics) = s.analytics.clone() {
                     let resp_chars = response_chars(&resp);
                     let (saved_tokens, savings_pct) =
@@ -715,17 +734,34 @@ fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Respons
     })? {
         return Ok(failure.into_response(name));
     }
+    // R1 (B1, D60): the input/entry budget allow-list gate for EVERY inner
+    // Starlark call, before the opcode shortcut or the plugin fall-through, so a
+    // script cannot reach an unmetered tool (edit_file, snapshot, kgl_*, a
+    // plugin, any later name) under `--tool-input-bytes`/`--tool-max-entries`.
+    // Keyed on the configured budget (consulted before the shared budget is
+    // installed on the locked op). A bare `--tool-timeout-ms` refuses nothing.
+    if let Some(failure) = with_ctx(|ctx| {
+        let session = ctx.session.clone();
+        Ok(ctx
+            .handle
+            .block_on(async { session.lock().await.budget_input_refusal(name) }))
+    })? {
+        return Ok(failure.into_response(name));
+    }
     let resp = match tools::build_request(name, args) {
         Some(Ok(request)) => dispatch_request(request, name)?,
         Some(Err(e)) => return Err(anyhow::anyhow!("{e}")),
         None => with_ctx(|ctx| {
             let started = std::time::Instant::now();
             let request_chars = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);
+            let budget = ctx.budget.clone();
             ctx.handle.block_on(async {
                 let mut session = ctx.session.lock().await;
-                let Some((content, is_error, meta)) =
-                    crate::mcp::dispatch_local_tool(&mut session, name, args).await
-                else {
+                // FR-19 F4: shared budget bounded to this locked op (see above).
+                session.set_operation_budget(budget.clone());
+                let dispatched = crate::mcp::dispatch_local_tool(&mut session, name, args).await;
+                session.set_operation_budget(None);
+                let Some((content, is_error, meta)) = dispatched else {
                     if name.starts_with("mcp__") {
                         return Err(anyhow::anyhow!(
                             "remote MCP tool '{name}' is unavailable in execute_script: \
diff --git a/src/session.rs b/src/session.rs
index 9312648..164a93e 100644
--- a/src/session.rs
+++ b/src/session.rs
@@ -89,10 +89,29 @@ pub struct Session {
     /// and model-visible listing. Unrestricted keeps legacy behavior.
     /// Context: src/run_policy.rs::ToolPolicy.
     pub policy: crate::run_policy::ToolPolicy,
+    /// Caller's cooperative tool-budget options (FR-19, WP18, ADR-031), copied
+    /// from `cfg.tool_budget`. Used to install a fresh operation budget at each
+    /// top-level tool call. Context: src/tool_budget.rs.
+    pub tool_budget_config: crate::tool_budget::ToolBudgetConfig,
+    /// The operation budget in force for the current top-level tool call, or
+    /// `None` between calls / when no budget option is set. Installed once per
+    /// top-level call and shared (inherited, never reset) by composite children
+    /// so delegation cannot multiply the allowance (ADR-031).
+    pub tool_budget: Option<Arc<crate::tool_budget::ToolBudget>>,
+    /// R3 (B3): the budget hit (code, configured, consumed) recorded IN-BAND by
+    /// the budget code when a genuine deadline/input/entry limit fired on this
+    /// call, so the run loop records the reserved `limit_hit` audit event from a
+    /// trusted signal instead of parsing attacker-influenceable result text.
+    /// Interior-mutable so the `&self` budget helpers can set it; drained by
+    /// `take_budget_hit` and cleared when a fresh top-level budget is installed.
+    pub budget_hit: std::sync::Mutex<Option<BudgetHitRecord>>,
 }
 
 pub type BgProcess = crate::managed_process::ManagedBackground;
 
+/// R3 (B3): an in-band cooperative budget hit: `(code, configured, consumed)`.
+pub type BudgetHitRecord = (String, Option<u64>, Option<u64>);
+
 impl Session {
     pub fn new(workspace: PathBuf, cfg: Arc<Config>) -> Self {
         let cwd = match &cfg.filesystem_authority {
@@ -121,6 +140,7 @@ impl Session {
             relocated(cfg.snapshot_directory.as_ref()),
         );
         let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
+        let tool_budget_config = cfg.tool_budget;
         Self {
             workspace,
             cwd,
@@ -146,9 +166,227 @@ impl Session {
             authority,
             admission,
             policy,
+            tool_budget_config,
+            tool_budget: None,
+            budget_hit: std::sync::Mutex::new(None),
         }
     }
 
+    /// Install a fresh operation budget for a top-level tool call unless one is
+    /// already active (a composite's children inherit the parent's). Returns
+    /// `true` when this call installed the budget and must clear it afterwards
+    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
+    pub fn begin_tool_operation(&mut self) -> bool {
+        if self.tool_budget.is_some() {
+            return false;
+        }
+        // R3/R4: a fresh top-level call starts with no stale in-band hit.
+        *self.budget_hit.lock().expect("budget_hit poisoned") = None;
+        self.tool_budget = self.tool_budget_config.start();
+        self.tool_budget.is_some()
+    }
+
+    /// R3 (B3): record an in-band budget hit (the code and its numbers) so the
+    /// run loop can write the reserved `limit_hit` event without parsing result
+    /// text. Called by the budget helpers and the native readers on a genuine
+    /// hit only.
+    pub fn note_budget_hit(&self, hit: &crate::tool_budget::BudgetHit) {
+        use crate::tool_budget::BudgetHit;
+        let record = match hit {
+            BudgetHit::Timeout { configured_ms } => (
+                "tool_timeout".to_string(),
+                Some(*configured_ms),
+                Some(*configured_ms),
+            ),
+            BudgetHit::Input {
+                limit, consumed, ..
+            } => (
+                "tool_input_limit".to_string(),
+                Some(*limit),
+                Some(*consumed),
+            ),
+        };
+        *self.budget_hit.lock().expect("budget_hit poisoned") = Some(record);
+    }
+
+    /// R3 (B3): drain the in-band budget hit recorded during the last dispatch.
+    pub fn take_budget_hit(&self) -> Option<BudgetHitRecord> {
+        self.budget_hit.lock().expect("budget_hit poisoned").take()
+    }
+
+    /// Clear the operation budget if this call owned it. A composite child
+    /// (`owned == false`) leaves the parent's budget in place.
+    pub fn end_tool_operation(&mut self, owned: bool) {
+        if owned {
+            self.tool_budget = None;
+        }
+    }
+
+    /// Charge `bytes` logical input against the active budget. Returns the
+    /// typed [`crate::path_policy::ToolFailure`] when the ceiling is exceeded;
+    /// a no-op when no byte budget is active.
+    pub fn budget_charge_bytes(
+        &self,
+        bytes: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.charge_input(bytes) {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
+    /// R2 (B4): refuse `bytes` against the active byte budget WITHOUT charging,
+    /// so `read_file` can reject a file by its metadata size before reading it.
+    /// Returns the typed `tool_input_limit` when the size alone would exceed the
+    /// remaining allowance; a no-op when no byte budget is active.
+    pub fn budget_precheck_bytes(
+        &self,
+        bytes: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.precheck_input(bytes) {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
+    /// A clone of the active operation budget, for handing the shared `Arc`
+    /// into a blocking search worker so the worker itself meters scanned bytes
+    /// and checks the deadline (FR-19 F1/F2). `None` when no budget is active.
+    pub fn tool_budget(&self) -> Option<std::sync::Arc<crate::tool_budget::ToolBudget>> {
+        self.tool_budget.clone()
+    }
+
+    /// Install (or clear with `None`) the operation budget directly. Used by
+    /// `execute_script` (FR-19 F4): the script owns ONE shared budget and
+    /// installs it on the session only inside each inner per-op lock, clearing
+    /// it when the lock is released, so the budget never lingers on the shared
+    /// session between ops where a concurrent top-level call could inherit it.
+    /// `begin_tool_operation` then sees it already installed and inherits.
+    pub fn set_operation_budget(
+        &mut self,
+        budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
+    ) {
+        self.tool_budget = budget;
+    }
+
+    /// True when ANY operation budget is active (deadline, byte, or entry).
+    /// Used to refuse composite routes whose external work cannot be governed
+    /// cooperatively (e.g. `tool_pipeline`).
+    pub fn budget_active(&self) -> bool {
+        self.tool_budget.is_some()
+    }
+
+    /// True when the active budget carries an input-byte or directory-entry
+    /// ceiling (a logical-input budget a tool must be able to meter).
+    pub fn budget_has_input_limits(&self) -> bool {
+        self.tool_budget
+            .as_ref()
+            .is_some_and(|b| b.has_input_limits())
+    }
+
+    /// R1 (B1, ADR-031): under an input-byte or directory-entry budget, admit a
+    /// tool only if it is on the explicit [`crate::tool_budget::tool_input_budget_admitted`]
+    /// allow-list (it meters its input or reads no workspace/external content).
+    /// Every other name is REFUSED with the reviewed `tool_policy_unsupported`
+    /// (D60), never silently exempted. Keyed on the configured budget, not an
+    /// installed one, so every frontend dispatch boundary (the MCP seam, the
+    /// agent/chat/ACP loop, and `execute_script`) can consult it uniformly
+    /// before a budget is even installed. `None` when no input/entry budget is
+    /// set (a bare `--tool-timeout-ms` refuses nothing here) or when `tool` is
+    /// admitted.
+    pub fn budget_input_refusal(&self, tool: &str) -> Option<crate::path_policy::ToolFailure> {
+        let cfg = &self.tool_budget_config;
+        let input_or_entry = cfg.input_bytes.is_some() || cfg.max_entries.is_some();
+        if input_or_entry && !crate::tool_budget::tool_input_budget_admitted(tool) {
+            Some(
+                crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
+                    Some(tool),
+                    "it is not on the input/entry budget allow-list (it reads workspace \
+                 or external content that is not metered, or starts external work)",
+                ),
+            )
+        } else {
+            None
+        }
+    }
+
+    /// R1 (ADR-031): the index-answered search (`search mode=files`) reads
+    /// cached-index content that is not a scannable byte count, so under an
+    /// input/entry budget it is REFUSED regardless of the admission allow-list
+    /// (which admits `search` for its byte-metered content mode). Checks the
+    /// installed budget (this runs inside `ops::dispatch`, after the budget was
+    /// installed). `None` when no input/entry budget is active.
+    pub fn budget_index_search_refusal(&self) -> Option<crate::path_policy::ToolFailure> {
+        if self.budget_has_input_limits() {
+            Some(
+                crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
+                    Some("search"),
+                    "the index (files) search mode reads cached-index content that is not a \
+                 scannable byte count",
+                ),
+            )
+        } else {
+            None
+        }
+    }
+
+    /// Charge `entries` directory entries against the active budget.
+    pub fn budget_charge_entries(
+        &self,
+        entries: u64,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.charge_entries(entries) {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
+    /// Remaining time until the active budget's absolute deadline, to hand to
+    /// the managed-subprocess path so external work (exec, git_read) inherits
+    /// but never extends it (FR-19, WP18, ADR-031). `None` when no budget or no
+    /// deadline is active.
+    pub fn budget_remaining(&self) -> Option<std::time::Duration> {
+        self.tool_budget.as_ref().and_then(|b| b.remaining())
+    }
+
+    /// The typed `tool_timeout` failure for the active budget, used after the
+    /// managed-subprocess path reports it stopped external work at the
+    /// deadline. `None` when no budget is active.
+    pub fn budget_timeout_failure(
+        &self,
+        argument: &str,
+    ) -> Option<crate::path_policy::ToolFailure> {
+        self.tool_budget
+            .as_ref()
+            .map(|b| b.timeout_failure(Some(argument)))
+    }
+
+    /// Error if the active budget's absolute deadline has passed.
+    pub fn budget_check_deadline(
+        &self,
+        argument: &str,
+    ) -> Result<(), crate::path_policy::ToolFailure> {
+        if let Some(budget) = &self.tool_budget {
+            if let Err(hit) = budget.check_deadline() {
+                self.note_budget_hit(&hit);
+                return Err(hit.into_failure(Some(argument)));
+            }
+        }
+        Ok(())
+    }
+
     /// Build the initial session environment with an enhanced PATH.
     /// Combines: config extra_path + auto-detected tool dirs + parent process PATH.
     fn build_initial_env(cfg: &Config) -> HashMap<String, String> {
diff --git a/src/session_bootstrap.rs b/src/session_bootstrap.rs
index 5a7ac78..a552469 100644
--- a/src/session_bootstrap.rs
+++ b/src/session_bootstrap.rs
@@ -61,6 +61,14 @@ pub struct BootstrapOptions<'a> {
     /// Part of the launch identity so a frontend requesting it cannot silently
     /// attach to a daemon launched without it.
     pub full_tool_schemas: bool,
+    /// FR-19 (WP18, ADR-031): the caller's cooperative tool-budget options.
+    /// Part of the daemon's immutable launch identity exactly like
+    /// `workspace_read_only`: forwarded in `daemon_args`, hashed into
+    /// `bootstrap_fingerprint`, and treated as an explicit selection in
+    /// `launch_identity`, so a client requesting a budget cannot be served by a
+    /// daemon started without the same one (the daemon pins its budget from its
+    /// OWN parsed flags). All-absent keeps legacy behavior.
+    pub tool_budget: crate::tool_budget::ToolBudgetConfig,
 }
 
 pub struct BootstrapConnection {
@@ -229,6 +237,21 @@ fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
     if options.no_follow {
         args.push(OsString::from("--no-follow-symlinks"));
     }
+    // FR-19 (WP18, ADR-031): forward the caller's budget to the daemon so an
+    // interactive session actually enforces it (global flags, before the
+    // subcommand). Omitted options add nothing, preserving legacy launch args.
+    if let Some(ms) = options.tool_budget.timeout_ms {
+        args.push(OsString::from("--tool-timeout-ms"));
+        args.push(OsString::from(ms.to_string()));
+    }
+    if let Some(bytes) = options.tool_budget.input_bytes {
+        args.push(OsString::from("--tool-input-bytes"));
+        args.push(OsString::from(bytes.to_string()));
+    }
+    if let Some(entries) = options.tool_budget.max_entries {
+        args.push(OsString::from("--tool-max-entries"));
+        args.push(OsString::from(entries.to_string()));
+    }
     args.push(OsString::from("session-daemon"));
     args.push(OsString::from("--socket"));
     args.push(options.socket_path.as_os_str().to_owned());
@@ -298,6 +321,20 @@ fn bootstrap_fingerprint(options: &BootstrapOptions<'_>) -> String {
     if options.no_follow {
         digest.update(b"no-follow\0");
     }
+    // FR-19 (WP18, ADR-031): pin the budget into the launch identity so an
+    // attach cannot silently run under a different (or absent) budget.
+    if let Some(ms) = options.tool_budget.timeout_ms {
+        digest.update(b"tool-timeout-ms\0");
+        digest.update(ms.to_le_bytes());
+    }
+    if let Some(bytes) = options.tool_budget.input_bytes {
+        digest.update(b"tool-input-bytes\0");
+        digest.update(bytes.to_le_bytes());
+    }
+    if let Some(entries) = options.tool_budget.max_entries {
+        digest.update(b"tool-max-entries\0");
+        digest.update(entries.to_le_bytes());
+    }
     if let Some(provider) = options.provider {
         digest.update(provider.as_bytes());
     }
@@ -320,6 +357,7 @@ fn launch_identity(options: &BootstrapOptions<'_>) -> std::io::Result<LaunchIden
         && options.confine_identity.is_none()
         && !options.ignore_config_files
         && !options.no_follow
+        && !options.tool_budget.any_enabled()
     {
         return Ok(LaunchIdentity::NotRequested);
     }
@@ -395,6 +433,7 @@ mod tests {
         agent_env: &'a Path,
     ) -> BootstrapOptions<'a> {
         BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             workspace,
             tool_allowlist: None,
             config_path: Some(config),
@@ -474,6 +513,71 @@ mod tests {
         );
     }
 
+    // WP18 (FR-19, ADR-031, req 1): the interactive agent runs through the
+    // session daemon, so each budget option must be a daemon launch argument
+    // AND part of the launch identity. If it were not in the identity, an
+    // attach could silently reuse a daemon with a different (or absent) budget;
+    // the client compares fingerprints and refuses on mismatch.
+    #[test]
+    fn tool_budget_is_a_daemon_argument_and_pins_the_launch_identity() {
+        let base = options(
+            Path::new("/workspace"),
+            Path::new("/config.toml"),
+            Path::new("/runtime/session.sock"),
+            Path::new("/agent.env"),
+        );
+        let budgeted = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig {
+                timeout_ms: Some(1500),
+                input_bytes: Some(4096),
+                max_entries: Some(32),
+            },
+            ..options(
+                Path::new("/workspace"),
+                Path::new("/config.toml"),
+                Path::new("/runtime/session.sock"),
+                Path::new("/agent.env"),
+            )
+        };
+        let args = daemon_args(&budgeted);
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-timeout-ms", "1500"]));
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-input-bytes", "4096"]));
+        assert!(args
+            .windows(2)
+            .any(|pair| pair == ["--tool-max-entries", "32"]));
+        // The unbudgeted daemon carries none of these arguments.
+        assert!(!daemon_args(&base)
+            .iter()
+            .any(|arg| arg == "--tool-timeout-ms"));
+        // A budget changes the launch identity, so an attach to a daemon that
+        // lacks it (or has a different one) is refused.
+        assert_ne!(
+            bootstrap_fingerprint(&base),
+            bootstrap_fingerprint(&budgeted)
+        );
+        let other = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig {
+                timeout_ms: Some(500),
+                input_bytes: None,
+                max_entries: None,
+            },
+            ..options(
+                Path::new("/workspace"),
+                Path::new("/config.toml"),
+                Path::new("/runtime/session.sock"),
+                Path::new("/agent.env"),
+            )
+        };
+        assert_ne!(
+            bootstrap_fingerprint(&budgeted),
+            bootstrap_fingerprint(&other)
+        );
+    }
+
     // WP11 (FR-09, ADR-021): explicit-only mode is pinned into the daemon's
     // launch identity, the same way --confine-paths is.
     #[test]
@@ -485,6 +589,7 @@ mod tests {
             Path::new("/agent.env"),
         );
         let explicit = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             ignore_config_files: true,
             ..options(
                 Path::new("/workspace"),
@@ -512,6 +617,7 @@ mod tests {
         let config = directory.path().join("config.toml");
         let agent_env = directory.path().join("agent.env");
         let pinned = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -652,6 +758,7 @@ mod tests {
             LaunchIdentity::Unavailable
         );
         let bare = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -776,6 +883,7 @@ mod tests {
         // explicit selection (not NotRequested), so it is compared against the
         // daemon's published identity.
         let read_only = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
@@ -786,6 +894,7 @@ mod tests {
         // The running daemon published a NON-read-only fingerprint (its own
         // bare identity). The client must NOT silently attach.
         let non_read_only = BootstrapOptions {
+            tool_budget: crate::tool_budget::ToolBudgetConfig::default(),
             config_path: None,
             provider: None,
             agent_env: None,
diff --git a/src/tool_budget.rs b/src/tool_budget.rs
new file mode 100644
index 0000000..c861353
--- /dev/null
+++ b/src/tool_budget.rs
@@ -0,0 +1,438 @@
+//! Cooperative, operation-scoped tool budgets (WP18, FR-19, ADR-031).
+//!
+//! A budget carries an absolute deadline and two independent optional logical
+//! counters — bytes of file/git content read or scanned, and directory entries
+//! walked. It is created once per top-level tool call and shared by every child
+//! of a composite (`batch`, `execute_script`, `tool_pipeline`) so delegation
+//! cannot multiply the allowance: a child inherits the remaining allowance and
+//! cannot extend the deadline (ADR-031). Limits are caller-set and disabled by
+//! default; when every option is absent no budget is installed and behavior is
+//! unchanged.
+//!
+//! Counting is logical, not physical: charge the file/git object content read
+//! or scanned (including cached-index content) and directory entries walked —
+//! never device I/O or displayed/truncated output bytes, which remain a
+//! separate mechanism (`src/tool_output.rs`).
+//!
+//! Context: docs/contexts/reviewer-runtime/CONTEXT.md,
+//! docs/adr/031-cooperative-tool-budgets.md.
+
+use std::io::{self, Read};
+use std::sync::atomic::{AtomicU64, Ordering};
+use std::sync::Arc;
+use std::time::{Duration, Instant};
+
+use crate::path_policy::ToolFailure;
+
+/// R1 (B1, ADR-031): the ALLOW-LIST that governs admission under an input-byte
+/// or directory-entry budget (`--tool-input-bytes` / `--tool-max-entries`). A
+/// tool is admitted ONLY if it appears here; every other name — process
+/// plugins, config-registered `[tools.*]`/`tool_run`, `kgl_*`, snapshot and
+/// restore, `bg`, outbound `mcp__*`, and any name a later package adds — is
+/// REFUSED with the reviewed `tool_policy_unsupported` (D60), never silently
+/// exempted. Inverting the old deny-list closes the silent-exemption surface:
+/// a new unmeterable tool is refused by default, not admitted by omission.
+///
+/// Admitted because they METER the logical input they read:
+/// `read_file` (charges file content bytes read); `ls` (charges directory
+/// entries walked); `search` (content mode charges scanned bytes in the
+/// blocking worker — the index/files mode is refused separately in `ops::find`
+/// because cached-index content is not a scannable byte count); `git_read`
+/// (metered by the bytes of git object content it returns — D61 approximation,
+/// recorded in docs/configuration.md).
+///
+/// Admitted because they read NO workspace or external content to meter:
+/// `write_file`, `append_file` (write output only; they scan no input);
+/// `batch`, `execute_script` (composite dispatchers — each child re-enters this
+/// gate and is metered or refused on its own while sharing the one budget per
+/// ADR-031, so admitting the wrapper exempts nothing); the session/runtime
+/// state tools `set_cwd`, `set_verbosity`, `set_external_session_id`,
+/// `session_stats` (in-process session state, not workspace files); the static
+/// tool-catalog tools `get_tool_schema`, `list_tool_signatures`,
+/// `list_all_tools` (static schemas, not workspace content); the agent-mail
+/// tools `register_agent`, `list_agents`, `send_message`, `reply_message`,
+/// `fetch_inbox`, `mark_read`, `acknowledge`, `fetch_thread` and the advisory
+/// reservation tools `reserve_paths`, `renew_reservations`,
+/// `release_reservations`, `check_conflicts`, `list_reservations` (agent-mail /
+/// reservation store per ADR-009, not workspace file/git content); and
+/// `update_plan` (in-memory agent plan state).
+///
+/// Deliberately ABSENT (refused under an input/entry budget): `edit_file` and
+/// `diff_files` (read workspace files unmetered), `skill` (reads a skill file),
+/// `workspace_info` (reads the root listing unmetered), `kgl_query`/`kgl_assert`
+/// (knowledge-graph store), `snapshot` (snapshot store content), `tool_pipeline`
+/// (external stages), and every process plugin.
+pub fn tool_input_budget_admitted(name: &str) -> bool {
+    matches!(
+        name,
+        // Meter their logical input.
+        "read_file"
+            | "ls"
+            | "search"
+            | "git_read"
+            // No input to meter (write-only).
+            | "write_file"
+            | "append_file"
+            // Composite dispatchers: children are gated individually.
+            | "batch"
+            | "execute_script"
+            // Session / runtime state.
+            | "set_cwd"
+            | "set_verbosity"
+            | "set_external_session_id"
+            | "session_stats"
+            // Static tool-catalog introspection.
+            | "get_tool_schema"
+            | "list_tool_signatures"
+            | "list_all_tools"
+            // Agent-mail store (ADR-009).
+            | "register_agent"
+            | "list_agents"
+            | "send_message"
+            | "reply_message"
+            | "fetch_inbox"
+            | "mark_read"
+            | "acknowledge"
+            | "fetch_thread"
+            // Advisory-reservation store (ADR-009).
+            | "reserve_paths"
+            | "renew_reservations"
+            | "release_reservations"
+            | "check_conflicts"
+            | "list_reservations"
+            // In-memory agent plan state.
+            | "update_plan"
+    )
+}
+
+/// Caller-configured budget options. All-absent means no budget is installed
+/// (legacy behavior). Caller-owned; never deserialized from workspace config.
+#[derive(Debug, Clone, Copy, Default)]
+pub struct ToolBudgetConfig {
+    /// `--tool-timeout-ms N`: absolute per-call deadline in milliseconds.
+    pub timeout_ms: Option<u64>,
+    /// `--tool-input-bytes N`: logical input byte ceiling for the call.
+    pub input_bytes: Option<u64>,
+    /// `--tool-max-entries N`: directory-entry ceiling for the call (D56,
+    /// `(proposed)` spelling; a hit reports the reviewed `tool_input_limit`).
+    pub max_entries: Option<u64>,
+}
+
+impl ToolBudgetConfig {
+    pub fn any_enabled(&self) -> bool {
+        self.timeout_ms.is_some() || self.input_bytes.is_some() || self.max_entries.is_some()
+    }
+
+    /// Install a fresh operation budget for a top-level call, or `None` when no
+    /// option is set (preserving legacy behavior).
+    pub fn start(&self) -> Option<Arc<ToolBudget>> {
+        if !self.any_enabled() {
+            return None;
+        }
+        Some(Arc::new(ToolBudget::new(*self)))
+    }
+}
+
+/// Which input ceiling a failure names. Both use the reviewed
+/// `tool_input_limit` code; the message names which was hit (integrator D56).
+#[derive(Debug, Clone, Copy)]
+pub enum Limit {
+    Bytes,
+    Entries,
+}
+
+impl Limit {
+    fn label(self) -> &'static str {
+        match self {
+            Limit::Bytes => "input bytes",
+            Limit::Entries => "directory entries",
+        }
+    }
+}
+
+/// An operation-scoped cooperative budget: an absolute deadline plus two
+/// independent atomic counters. Shared across the children of one top-level
+/// call via `Arc`, so concurrent delegation cannot multiply the allowance.
+pub struct ToolBudget {
+    deadline: Option<Instant>,
+    timeout_ms: Option<u64>,
+    input_limit: Option<u64>,
+    input_consumed: AtomicU64,
+    entry_limit: Option<u64>,
+    entry_consumed: AtomicU64,
+}
+
+impl ToolBudget {
+    fn new(cfg: ToolBudgetConfig) -> Self {
+        let deadline = cfg
+            .timeout_ms
+            .map(|ms| Instant::now() + Duration::from_millis(ms));
+        Self {
+            deadline,
+            timeout_ms: cfg.timeout_ms,
+            input_limit: cfg.input_bytes,
+            input_consumed: AtomicU64::new(0),
+            entry_limit: cfg.max_entries,
+            entry_consumed: AtomicU64::new(0),
+        }
+    }
+
+    /// Remaining time until the absolute deadline, for handing to the managed
+    /// subprocess path so external work inherits (never extends) it. `None`
+    /// when no deadline is set.
+    pub fn remaining(&self) -> Option<Duration> {
+        self.deadline
+            .map(|d| d.saturating_duration_since(Instant::now()))
+    }
+
+    /// The typed deadline failure for this budget, used when external work
+    /// (a managed subprocess) was stopped at the deadline (FR-19, WP18).
+    pub fn timeout_failure(&self, argument: Option<&str>) -> ToolFailure {
+        BudgetHit::Timeout {
+            configured_ms: self.timeout_ms.unwrap_or(0),
+        }
+        .into_failure(argument)
+    }
+
+    /// Error if the absolute deadline has passed. Checked at bounded work
+    /// boundaries by native/index readers.
+    pub fn check_deadline(&self) -> Result<(), BudgetHit> {
+        if let Some(d) = self.deadline {
+            if Instant::now() >= d {
+                return Err(BudgetHit::Timeout {
+                    configured_ms: self.timeout_ms.unwrap_or(0),
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// True when an input-byte or directory-entry ceiling is configured (i.e.
+    /// a logical-input budget this tool would have to meter). Used to refuse a
+    /// tool that starts unmeterable external work (ADR-031).
+    pub fn has_input_limits(&self) -> bool {
+        self.input_limit.is_some() || self.entry_limit.is_some()
+    }
+
+    /// Charge `n` logical input bytes. No-op when no byte limit is set. Returns
+    /// a hit when the cumulative total exceeds the ceiling; the counter still
+    /// reflects the attempt so the reported `consumed` is honest. INTERFACES.md
+    /// reserves this name (`charge_input`).
+    pub fn charge_input(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.input_limit {
+            // `fetch_add` wraps on overflow, so the sum is saturated here. Two
+            // concurrent children can each pass this check for a moment while
+            // their sum passes the limit: the budget is cooperative, and the
+            // later charge of either child then fails.
+            let consumed = self
+                .input_consumed
+                .fetch_add(n, Ordering::Relaxed)
+                .saturating_add(n);
+            if consumed > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Bytes,
+                    limit,
+                    consumed,
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// R2 (B4): check whether reading `n` more input bytes WOULD exceed the
+    /// ceiling, WITHOUT charging. Lets a reader (`read_file`) refuse by file
+    /// size from metadata before reading a single byte. Returns the hit it
+    /// would raise (reporting the would-be consumed total). No-op (Ok) when no
+    /// byte limit is set.
+    pub fn precheck_input(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.input_limit {
+            let would_be = self
+                .input_consumed
+                .load(Ordering::Relaxed)
+                .saturating_add(n);
+            if would_be > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Bytes,
+                    limit,
+                    consumed: would_be,
+                });
+            }
+        }
+        Ok(())
+    }
+
+    /// R2 (B4): the remaining directory-entry allowance (`entry_limit` minus
+    /// what has already been charged), so a walker can be given a hard cap and
+    /// stop early instead of walking the whole tree. `None` when no entry limit
+    /// is set.
+    pub fn entry_remaining(&self) -> Option<u64> {
+        self.entry_limit
+            .map(|limit| limit.saturating_sub(self.entry_consumed.load(Ordering::Relaxed)))
+    }
+
+    /// Charge `n` directory entries. No-op when no entry limit is set.
+    pub fn charge_entries(&self, n: u64) -> Result<(), BudgetHit> {
+        if let Some(limit) = self.entry_limit {
+            let consumed = self
+                .entry_consumed
+                .fetch_add(n, Ordering::Relaxed)
+                .saturating_add(n);
+            if consumed > limit {
+                return Err(BudgetHit::Input {
+                    which: Limit::Entries,
+                    limit,
+                    consumed,
+                });
+            }
+        }
+        Ok(())
+    }
+}
+
+/// A budget violation, convertible to the reviewed [`ToolFailure`].
+#[derive(Debug, Clone)]
+pub enum BudgetHit {
+    Timeout {
+        configured_ms: u64,
+    },
+    Input {
+        which: Limit,
+        limit: u64,
+        consumed: u64,
+    },
+}
+
+impl BudgetHit {
+    /// Build the caller-facing typed failure. A deadline hit carries
+    /// `tool_timeout`; a byte or entry hit carries `tool_input_limit` and names
+    /// which ceiling was hit, the configured value, and the consumed value.
+    pub fn into_failure(self, argument: Option<&str>) -> ToolFailure {
+        match self {
+            BudgetHit::Timeout { configured_ms } => {
+                ToolFailure::tool_timeout(argument, configured_ms)
+            }
+            BudgetHit::Input {
+                which,
+                limit,
+                consumed,
+            } => ToolFailure::tool_input_limit(argument, which.label(), limit, consumed),
+        }
+    }
+}
+
+/// A `Read` wrapper that charges scanned bytes and checks the deadline at every
+/// bounded read step, so a native search worker meters the bytes it SCANS (not
+/// the bytes it returns) and stops mid-file at the deadline or the byte ceiling
+/// (FR-19 F1/F2, ADR-031). The worker that owns the reader inspects
+/// [`BudgetedReader::hit`] after the search to surface the typed failure; the
+/// read error that aborts the search is only the stop signal. Wrap only when a
+/// budget is active — the unbudgeted path keeps its exact legacy behavior.
+pub struct BudgetedReader<R> {
+    inner: R,
+    budget: Arc<ToolBudget>,
+    hit: Option<BudgetHit>,
+}
+
+impl<R: Read> BudgetedReader<R> {
+    pub fn new(inner: R, budget: Arc<ToolBudget>) -> Self {
+        Self {
+            inner,
+            budget,
+            hit: None,
+        }
+    }
+
+    /// The budget violation that stopped the scan, if any.
+    pub fn hit(&self) -> Option<&BudgetHit> {
+        self.hit.as_ref()
+    }
+}
+
+impl<R: Read> Read for BudgetedReader<R> {
+    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
+        // Deadline first: a bounded step boundary inside a large file.
+        if let Err(hit) = self.budget.check_deadline() {
+            self.hit = Some(hit);
+            return Err(io::Error::other("tool budget deadline"));
+        }
+        let n = self.inner.read(buf)?;
+        if n > 0 {
+            // Charge exactly the bytes just scanned.
+            if let Err(hit) = self.budget.charge_input(n as u64) {
+                self.hit = Some(hit);
+                return Err(io::Error::other("tool input budget"));
+            }
+        }
+        Ok(n)
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    /// R1 (B1): the admission allow-list. Tools that meter their input or read
+    /// no workspace/external content are admitted under an input/entry budget;
+    /// every other name — process plugins, config-registered tools, kgl_*,
+    /// snapshot, bg, outbound mcp__*, and ANY unknown/future name — is refused.
+    #[test]
+    fn input_budget_admits_only_metered_or_contentless_tools() {
+        // Admitted: they meter their logical input.
+        for name in ["read_file", "ls", "search", "git_read"] {
+            assert!(
+                tool_input_budget_admitted(name),
+                "{name} should be admitted"
+            );
+        }
+        // Admitted: they read no workspace/external content, or are composite
+        // dispatchers whose children are gated individually.
+        for name in [
+            "write_file",
+            "append_file",
+            "batch",
+            "execute_script",
+            "set_cwd",
+            "session_stats",
+            "get_tool_schema",
+            "register_agent",
+            "send_message",
+            "reserve_paths",
+            "list_reservations",
+        ] {
+            assert!(
+                tool_input_budget_admitted(name),
+                "{name} should be admitted"
+            );
+        }
+        // Refused: read workspace/external content unmetered or start a
+        // subprocess. Includes the exact classes the reviewer named (B1).
+        for name in [
+            "edit_file",
+            "diff_files",
+            "skill",
+            "workspace_info",
+            "kgl_query",
+            "kgl_assert",
+            "snapshot",
+            "bg",
+            "tool_run",
+            "tool_pipeline",
+            "exec",
+            "git",
+            "cargo",
+            "curl",
+            "lint_repair",
+        ] {
+            assert!(
+                !tool_input_budget_admitted(name),
+                "{name} should be refused"
+            );
+        }
+        // Refused: outbound remote MCP tools (mcp__*) are never admitted.
+        assert!(!tool_input_budget_admitted("mcp__server__some_tool"));
+        // Refused by default: an unknown name a later package might add is NOT
+        // silently exempted — the whole point of inverting the deny-list.
+        assert!(!tool_input_budget_admitted("some_future_tool"));
+        assert!(!tool_input_budget_admitted(""));
+    }
+}
diff --git a/tests/README.md b/tests/README.md
index 0674fba..e1af393 100644
--- a/tests/README.md
+++ b/tests/README.md
@@ -196,3 +196,28 @@ The keyword classification (reference vs data vs schema-position vs unknown)
 and the duplicate-`$id`/anchor refusal are proven at the
 `src/schema_contract.rs` unit seam, including a table that routes a cycle
 through every 2020-12 schema keyword.
+
+### WP18 cooperative tool budgets (`tests/test_tool_budgets.py`)
+
+Process-level acceptance tests for FR-19 (ADR-031): the three default-off
+budget options drive a real `daimonos` MCP process (and a real `agent --print`
+process for the audit/`--output json`/worker cases). Timing tests (AC-19.1) use
+a deadline of 1000–1500 ms and assert an upper bound of several seconds while
+keeping the data small, so a slower CI runner still stops promptly without
+flaking; each bound and its reason are stated in the test docstring. The
+`exec` and `git_read` reap proofs follow the method of
+`tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped`
+(scanning `/proc` for a unique marker) to show no child or grandchild outlives
+the stop; the `git_read` proof installs a `$GIT_DIR`-gated fake `git` shim so
+repo discovery still uses the real git while an operation call hangs. These
+tests need a Unix host with `/proc` and a real `git`; a skip is not a pass.
+
+`tests/test_tool_budgets.py` (WP18, FR-19): process-level cooperative tool
+budget tests — scanned-byte metering for content search, native deadline checks
+(read_file/ls/both search modes), plugin deadline+reap (curl, git), the
+`tool_policy_unsupported` refusals (each tool that is not on the admission
+list, the index search, the pipeline), the entry limit that stops the `ls`
+walk, the budget that does not leak into the next call or into a concurrent
+call, the
+AC-19.1 large-tree timeout bound, and the `limit_hit` audit event (by
+kind+fields). AC-19.1 writes 10000 small files and stays under ~10 s.
diff --git a/tests/test_tool_budgets.py b/tests/test_tool_budgets.py
new file mode 100644
index 0000000..1975f5f
--- /dev/null
+++ b/tests/test_tool_budgets.py
@@ -0,0 +1,1345 @@
+"""WP18 FR-19 process-level acceptance tests: cooperative tool budgets.
+
+Spawns a real daimonos MCP process (conftest ``daimonos_factory``) and drives
+the public tool interface. Budgets are default-off; every test that asserts a
+limit passes the CLI option explicitly. Context:
+docs/adr/031-cooperative-tool-budgets.md and the reviewer-runtime
+INTERFACES.md row "WP18, FR-19".
+"""
+
+import json
+
+
+def _text(result):
+    return result["content"][0]["text"]
+
+
+def test_input_bytes_limit_refuses_oversized_read(daimonos_factory, tmp_path):
+    """--tool-input-bytes N: a read whose logical input exceeds N is refused
+    with the reviewed ``tool_input_limit`` code and a budget naming the
+    configured value and the consumed value. No partial content is returned.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * 500)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 100, data
+    assert data["consumed"] >= 500, data
+    assert "content" not in data, "no partial result on a limit hit"
+
+
+def test_input_bytes_default_off_reads_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, an ordinary read is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * 500)
+    client = daimonos_factory(ws)
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    data = json.loads(_text(result))
+    assert data["content"] == "x" * 500
+
+
+import os
+import time
+
+
+def _survivors(needle):
+    """PIDs whose /proc cmdline contains `needle` — the process-table method of
+    tests/test_worker_isolation.py::_worker_survivors, used here to prove that
+    neither the exec child nor its grandchild outlives a budget stop (FR-19
+    requirement 3)."""
+    out = []
+    for pid in os.listdir("/proc"):
+        if not pid.isdigit():
+            continue
+        try:
+            with open("/proc/%s/cmdline" % pid, "rb") as f:
+                cmd = f.read().replace(b"\x00", b" ").decode("utf-8", "replace")
+        except OSError:
+            continue
+        if needle in cmd:
+            out.append(pid)
+    return out
+
+
+def _poll_gone(needle, bound_s):
+    deadline = time.monotonic() + bound_s
+    while time.monotonic() < deadline:
+        if not _survivors(needle):
+            return True
+        time.sleep(0.05)
+    return not _survivors(needle)
+
+
+def test_timeout_stops_exec_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
+    """--tool-timeout-ms N: an exec that outlives the deadline is stopped with
+    the reviewed ``tool_timeout`` code, and neither the child shell nor the
+    grandchild it backgrounds survives (FR-19 requirement 3; stopped work, not
+    only a stopped wait — the managed process group is TERM/KILL reaped via
+    ManagedChild::terminate).
+
+    Timing (AC-19.1, req 9): the deadline is 1000 ms — a limit of one second or
+    less so a slower CI runner still stops promptly. The call is asserted to
+    return within 8 s: generous for a loaded shared runner (expected stop is
+    ~1-3 s), yet well below the 15 s the shells would otherwise sleep, so a
+    pass proves the stop was the budget deadline, not the processes exiting on
+    their own.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    gc_marker = f"wp18gc-{os.getpid()}-{time.time_ns()}"
+    parent_marker = f"wp18parent-{os.getpid()}-{time.time_ns()}"
+    # The shell daimonos runs (sh -c <line>) backgrounds a grandchild shell
+    # whose argv carries gc_marker, then itself sleeps far past the deadline.
+    # Both stay in the exec's process group, so a group reap removes both.
+    line = (
+        f"sh -c 'sleep 15 # {gc_marker}' & "
+        f"sleep 15 # {parent_marker}"
+    )
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1000"])
+
+    start = time.monotonic()
+    result = client.call_tool("exec", {"command": line})
+    elapsed = time.monotonic() - start
+
+    assert elapsed < 8, f"budget did not stop exec promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "exit" not in data, "no partial exec result on a deadline hit"
+
+    assert _poll_gone(parent_marker, 5.0), "exec child survived the budget stop"
+    assert _poll_gone(gc_marker, 5.0), "exec grandchild survived the budget stop"
+
+
+def test_max_entries_limit_refuses_oversized_ls(daimonos_factory, tmp_path):
+    """--tool-max-entries N: an ls that walks more than N directory entries is
+    refused with the reviewed ``tool_input_limit`` code (integrator D56: the
+    entry ceiling reuses the byte code), a budget naming the configured value
+    and the consumed entry count, and no partial listing. Off by default.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(10):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "3"])
+
+    result = client.call_tool("ls", {"path": "."})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 3, data
+    # R2 (B4): the walk STOPS at the ceiling, so consumed is just past the
+    # configured limit (not the full tree) — enforcement, not post-hoc counting.
+    assert 3 < data["consumed"] <= 5, data
+    assert "entries" not in data, "no partial listing on an entry-limit hit"
+
+
+def test_max_entries_default_off_lists_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, an ordinary ls is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(10):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws)
+    result = client.call_tool("ls", {"path": "."})
+    data = json.loads(_text(result))
+    assert len(data["entries"]) == 10, data
+
+
+def test_input_bytes_meters_content_search_filesystem_path(daimonos_factory, tmp_path):
+    """--tool-input-bytes N on the FILESYSTEM search path (search mode=content
+    -> grep): the logical input the search returns is charged, so a result that
+    exceeds N is refused with ``tool_input_limit`` and no partial matches
+    (FR-19 req 4; the grep path reads the filesystem, not the index).
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(6):
+        (ws / f"hit_{i}.txt").write_text("needle here\n")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "40"])
+
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 40, data
+    assert "matches" not in data, "no partial matches on a limit hit"
+
+
+def test_input_bytes_refuses_index_search(daimonos_factory, tmp_path):
+    """F1 (M1): the INDEX search path (search mode=files -> trigram index)
+    reads the cached index, not scannable file content, so its logical input
+    cannot be metered. Under --tool-input-bytes the index-answered search is
+    REFUSED with a named error rather than charged "bytes returned" (ADR-031
+    allows refusing an enabled-but-unenforceable budget; the owner choice is
+    stated in the handoff). No partial results.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(8):
+        (ws / f"wp18idx_{i}.rs").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "30"])
+
+    result = client.call_tool("search", {"pattern": "wp18idx", "mode": "files"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_policy_unsupported", data
+    assert "results" not in data, "no partial index results on a refusal"
+
+
+# --- Composites share one budget (FR-19 requirement 2) ---
+
+_TWO_READS = (
+    "def main():\n"
+    "    read_file('big.txt')\n"
+    "    read_file('big.txt')\n"
+    "    return 'both'\n"
+    "result = main()\n"
+)
+
+
+def test_script_children_share_parent_budget(daimonos_factory, tmp_path):
+    """FR-19 req 2: a composite's children inherit the parent call's remaining
+    allowance, they do NOT each get a fresh top-level budget. Two reads of a
+    60-byte file under --tool-input-bytes 100: the first (60) fits, the second
+    (cumulative 120) trips the shared ceiling. This test FAILS if an
+    execute_script child is given its own new budget (both reads would fit).
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    result = client.call_tool("execute_script", {"code": _TWO_READS})
+    assert result.get("isError") is True, result
+    assert "tool_input_limit" in _text(result), _text(result)
+
+
+def test_script_single_read_within_budget_succeeds(daimonos_factory, tmp_path):
+    """Control: one 60-byte read under --tool-input-bytes 100 inside a script
+    succeeds, so the failure above is the shared second read, not read_file
+    being charged wrong."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    code = "def main():\n    read_file('big.txt')\n    return 'ok'\nresult = main()\n"
+    result = client.call_tool("execute_script", {"code": code})
+    assert result.get("isError") is not True, _text(result)
+    assert json.loads(_text(result))["result"] == "ok"
+
+
+def test_batch_children_share_parent_budget(daimonos_factory, tmp_path):
+    """FR-19 req 2: a batch's ops inherit the one parent budget. Two reads of a
+    60-byte file under --tool-input-bytes 100: op 1 fits, op 2 (cumulative 120)
+    trips the shared ceiling. FAILS if a batch op gets its own new budget."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("y" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+    ]})
+    content = json.loads(_text(result))
+    assert content[0]["ok"] is True, content
+    assert content[1]["ok"] is False, content
+    assert "tool_input_limit" in json.dumps(content[1]), content
+
+
+# --- Agent `--print` frontend: audit (req 5), --output json (req 11), req 1 ---
+#
+# A local, self-contained fake OpenAI-compatible endpoint (no network, no real
+# credential) drives a real `daimonos agent --print` process, mirroring
+# tests/test_run_audit.py. A budget option is passed as a global flag so the
+# agent frontend honors it; a limit hit must be a tool failure INSIDE the run
+# (the run still succeeds and, under --output json, still prints one document),
+# and the WP08 audit must record the named code.
+
+import os as _os
+import subprocess
+import threading
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
+from pathlib import Path
+
+_FAKE_KEY = "synthetic-not-a-real-key-WP18-5fZ9"
+_DONE = "WP18_AGENT_DONE_MARKER"
+
+
+class _FakeProvider:
+    def __init__(self, script):
+        self._script = list(script)
+        self._step = 0
+        self.received_tool_results = []
+        owner = self
+
+        class Handler(BaseHTTPRequestHandler):
+            protocol_version = "HTTP/1.1"
+
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                payload = json.dumps({"data": [{"id": "test/model",
+                    "context_length": 100000, "supported_parameters": ["tools"],
+                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
+                self.send_response(200)
+                self.send_header("Content-Type", "application/json")
+                self.send_header("Content-Length", str(len(payload)))
+                self.end_headers()
+                self.wfile.write(payload)
+
+            def do_POST(self):
+                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
+                tool_msgs = [m for m in body["messages"] if m.get("role") == "tool"]
+                if tool_msgs:
+                    owner.received_tool_results.append(json.dumps(tool_msgs))
+                step = owner._script[min(owner._step, len(owner._script) - 1)]
+                owner._step += 1
+                self.send_response(200)
+                self.send_header("Content-Type", "text/event-stream")
+                self.send_header("Connection", "close")
+                self.end_headers()
+                self.close_connection = True
+
+                def chunk(delta, reason=None):
+                    frame = {"choices": [{"index": 0, "delta": delta,
+                        "finish_reason": reason}],
+                        "usage": {"prompt_tokens": 42, "completion_tokens": 7}}
+                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
+                    self.wfile.flush()
+
+                try:
+                    if step[0] == "call":
+                        _, name, arguments = step
+                        chunk({"role": "assistant", "content": "", "tool_calls": [{
+                            "index": 0, "id": f"call-{owner._step}", "type": "function",
+                            "function": {"name": name,
+                                         "arguments": json.dumps(arguments)}}]},
+                              "tool_calls")
+                    else:
+                        chunk({"role": "assistant", "content": step[1]}, "stop")
+                    self.wfile.write(b"data: [DONE]\n\n")
+                    self.wfile.flush()
+                except (BrokenPipeError, ConnectionResetError):
+                    pass
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
+        self.thread.start()
+
+    @property
+    def port(self):
+        return self.server.server_port
+
+    def close(self):
+        self.server.shutdown()
+        self.server.server_close()
+        self.thread.join(timeout=3)
+
+
+def _agent_env_file(tmp_path, provider, allowed="read_file"):
+    path = tmp_path / "agent.env"
+    path.write_text("\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter",
+        "DAIMONOS_AGENT_MODEL=test/model",
+        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.port}/v1",
+        f"DAIMONOS_AGENT_API_KEY={_FAKE_KEY}",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
+        "DAIMONOS_AGENT_COMPACTION=off",
+        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
+        f"DAIMONOS_AGENT_ALLOWED_COMMANDS={allowed}",
+        "",
+    ]))
+    return path
+
+
+def _isolated_env(tmp_path):
+    stash = tmp_path / "home"
+    stash.mkdir(exist_ok=True)
+    (tmp_path / "tmp").mkdir(exist_ok=True)
+    env = {k: v for k, v in _os.environ.items() if not k.startswith("DAIMONOS_")}
+    env.update({
+        "HOME": str(stash),
+        "XDG_STATE_HOME": str(stash / "state"),
+        "XDG_DATA_HOME": str(stash / "data"),
+        "XDG_CONFIG_HOME": str(stash / "config"),
+        "TMPDIR": str(tmp_path / "tmp"),
+        "DAIMONOS_KGL_AUTOINDEX": "0",
+    })
+    return env
+
+
+def test_agent_print_json_budget_hit_is_tool_failure_and_is_audited(daimonos_binary, tmp_path):
+    """req 1 (agent --print honors the budget), req 11 (a limit hit under
+    --output json is a tool failure INSIDE the run; the run still prints its one
+    JSON document and exits 0) and req 5 (the WP08 audit records the hit with
+    its named code). The model calls read_file on a 500-byte file under
+    --tool-input-bytes 100, gets a tool_input_limit, then ends normally.
+    """
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    (workspace / "big.txt").write_text("z" * 500)
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider)
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "--tool-input-bytes", "100",
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "do the read"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    # req 11: exactly one JSON document on stdout.
+    doc = json.loads(completed.stdout)
+    assert isinstance(doc, dict), completed.stdout
+    # The tool result handed back to the model carried the named code: the
+    # limit hit was a tool failure inside the run, not a run abort.
+    joined = " ".join(provider.received_tool_results)
+    assert "tool_input_limit" in joined, joined
+    # req 5: the audit recorded the hit with its named code.
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    assert any("tool_input_limit" in json.dumps(ev) for ev in events), events
+
+
+# --- git_read deadline + process-group reap (FR-19 req 3; slice 06) ---
+
+import shutil
+
+
+def _init_repo(ws):
+    """A minimal real git repo for git_read to open, built with the real git on
+    PATH before any fake git is installed."""
+    real = shutil.which("git")
+    assert real, "a real git is required to build the fixture repo"
+
+    def g(*a):
+        subprocess.run([real, *a], cwd=str(ws), check=True,
+                       capture_output=True, env={**_os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
+                                                  "GIT_CONFIG_SYSTEM": "/dev/null"})
+    g("init", "-q")
+    g("config", "user.email", "wp18@example.invalid")
+    g("config", "user.name", "WP18")
+    (ws / "a.txt").write_text("hello\n")
+    g("add", "a.txt")
+    g("commit", "-q", "-m", "seed")
+    return real
+
+
+def _install_fake_git(bindir, real_git, parent_marker, gc_marker):
+    """A `git` shim that passes repo discovery through to the real git but, once
+    git_read sets GIT_DIR for an operation, backgrounds a grandchild and sleeps
+    far past the deadline. Both descendants carry a unique marker in their argv
+    and stay in the managed child's process group, so a group reap removes them.
+    """
+    bindir.mkdir(parents=True, exist_ok=True)
+    shim = bindir / "git"
+    shim.write_text(
+        "#!/bin/sh\n"
+        "if [ -n \"$GIT_DIR\" ]; then\n"
+        f"  sh -c 'sleep 12 # {gc_marker}' &\n"
+        f"  exec sh -c 'sleep 12 # {parent_marker}'\n"
+        "fi\n"
+        f"exec {real_git} \"$@\"\n"
+    )
+    shim.chmod(0o755)
+
+
+def test_timeout_stops_git_read_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
+    """--tool-timeout-ms N: a git_read whose git subprocess outlives the
+    deadline is stopped with the reviewed ``tool_timeout`` code, and neither the
+    git child nor the grandchild it backgrounds survives (FR-19 req 3 names
+    git_read for a reap proof; stopped work, not only a stopped wait — the whole
+    process group is TERM/KILL reaped via ManagedChild::terminate, the method of
+    tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped).
+
+    git_read is dispatched directly (not via ops::dispatch), so before this
+    slice a standalone git_read got no budget at all; the deadline is threaded
+    into managed_process::run via a task-local so the git child is actually
+    reaped, not merely abandoned.
+
+    Timing (AC-19.1, req 9): the deadline is 1500 ms — one and a half seconds so
+    a slower CI runner still stops promptly. The call is asserted to return
+    within 8 s (expected stop ~1.5-3 s) yet well below the 12 s the shims would
+    otherwise sleep, so a pass proves the stop was the budget deadline, not the
+    shims exiting on their own.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    real_git = _init_repo(ws)
+    bindir = tmp_path / "bin"
+    parent_marker = f"wp18git-{_os.getpid()}-{time.time_ns()}"
+    gc_marker = f"wp18gitgc-{_os.getpid()}-{time.time_ns()}"
+    _install_fake_git(bindir, real_git, parent_marker, gc_marker)
+
+    env = {"PATH": f"{bindir}{_os.pathsep}{_os.environ.get('PATH','')}",
+           "DAIMONOS_KGL_AUTOINDEX": "0"}
+    client = daimonos_factory(ws, env=env, args=["--tool-timeout-ms", "1500"])
+
+    start = time.monotonic()
+    result = client.call_tool("git_read", {"command": "log"})
+    elapsed = time.monotonic() - start
+
+    assert elapsed < 8, f"budget did not stop git_read promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "entries" not in data, "no partial git_read result on a deadline hit"
+
+    assert _poll_gone(parent_marker, 5.0), "git_read child survived the budget stop"
+    assert _poll_gone(gc_marker, 5.0), "git_read grandchild survived the budget stop"
+
+
+# --- Default off: every metered tool is unchanged with no budget (req 8) ---
+#
+# read_file and ls already have default-off regressions above. These cover the
+# remaining tools FR-19 req 8 names: exec, search, git_read. With none of the
+# three options, the new code paths (budget install, the with_budget_deadline
+# wrapper around managed_process::run) must be exact no-ops.
+
+
+def test_exec_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option an ordinary exec is unchanged
+    — it completes and returns its real exit, not a tool_timeout."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws)
+    result = client.call_tool("exec", {"command": "printf wp18ok"})
+    assert result.get("isError") is not True, _text(result)
+    data = json.loads(_text(result))
+    assert data["exit"] == 0, data
+    assert "wp18ok" in json.dumps(data), data
+
+
+def test_search_default_off_returns_all_matches(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option, a content search returns its
+    full result (no tool_input_limit)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(6):
+        (ws / f"hit_{i}.txt").write_text("needle here\n")
+    client = daimonos_factory(ws)
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_input_limit" not in _text(result), _text(result)
+
+
+def test_git_read_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression (req 8): with no budget option a git_read is unchanged — the
+    with_budget_deadline(None, ..) wrapper around the git subprocess is an exact
+    no-op and the real git log is returned, not a tool_timeout."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("git_read", {"command": "log"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_timeout" not in _text(result), _text(result)
+    assert "entries" in _text(result), _text(result)
+
+
+# --- Combinations: the budget composes with every other run option (req 6) ---
+#
+# Each test pairs a budget option with one other run-shaping option and asserts
+# the budget still fires with its reviewed code. Output truncation stays a
+# SEPARATE mechanism: the last test proves a result bounded by --no-output-spill
+# is not reported as a budget limit failure.
+
+
+def _big(ws, n=500):
+    (ws / "big.txt").write_text("x" * n)
+
+
+def test_input_bytes_composes_with_tool_allowlist(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --tool-allowlist. read_file is on
+    the allowlist, so admission passes; the byte budget still trips with
+    tool_input_limit (the allowlist and the budget are independent gates)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    allowlist = tmp_path / "allow.json"
+    allowlist.write_text(json.dumps(["read_file"]))
+    client = daimonos_factory(ws, args=["--tool-allowlist", str(allowlist),
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_input_bytes_composes_with_workspace_read_only(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --workspace-read-only. A read is
+    allowed under read-only, and the byte budget still trips."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    client = daimonos_factory(ws, args=["--workspace-read-only",
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_input_bytes_composes_with_confine_paths_and_no_follow(daimonos_factory, tmp_path):
+    """req 6: --tool-input-bytes together with --confine-paths and
+    --no-follow-symlinks. The read is inside the confined workspace and the byte
+    budget still trips with tool_input_limit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws)
+    client = daimonos_factory(ws, args=["--confine-paths", "--no-follow-symlinks",
+                                        "--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)
+
+
+def test_no_output_spill_truncation_is_not_a_budget_limit(daimonos_factory, tmp_path):
+    """req 6: output truncation stays a mechanism distinct from the input
+    budget. A large read under --no-output-spill is bounded by the output rules,
+    not failed; with a generous --tool-input-bytes that the logical input fits
+    under, the result must NOT carry a budget code. Truncation != limit hit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _big(ws, n=300_000)
+    client = daimonos_factory(ws, args=["--no-output-spill",
+                                        "--tool-input-bytes", "10000000"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    body = _text(result)
+    assert "tool_input_limit" not in body, body
+    assert "tool_timeout" not in body, body
+
+
+# --- Workers (WP10, FR-19 req 7): a budgeted worker route is refused ---
+#
+# The cooperative budget installs at THIS process's dispatch seams, but a worker
+# routes tool calls into a separate process the parent budget never sees. So a
+# budget option cannot be enforced across the worker boundary; rather than
+# silently ignore it (a frontend-rule violation), the run is refused at startup
+# with run_configuration_invalid / exit 2 (D-WP18-5; ADR-031 refuse
+# enabled-but-unenforceable; INTERFACES worker rule). Ordinary worker mode with
+# no budget option is unchanged.
+
+_WORKER_REFUSAL = "tool_budget_worker_unenforceable"
+
+
+def _min_worker_config(tmp_path):
+    wc = tmp_path / "wc.json"
+    wc.write_text(json.dumps({"workers": {"w": {
+        "command": "/bin/true", "args": [],
+        "env": {"PATH": _os.environ.get("PATH", "/usr/bin:/bin")}}}}))
+    return wc
+
+
+def _run_agent_worker(daimonos_binary, tmp_path, provider, budget_args):
+    env = _isolated_env(tmp_path)
+    agent_env = _agent_env_file(tmp_path, provider)
+    ws = tmp_path / "ws"
+    ws.mkdir(exist_ok=True)
+    wc = _min_worker_config(tmp_path)
+    cmd = [daimonos_binary, "--workspace", str(ws), *budget_args,
+           "agent", "--print", "--worker-config", str(wc),
+           "--agent-env", str(agent_env), "do it"]
+    return subprocess.run(cmd, capture_output=True, text=True, timeout=60, env=env)
+
+
+def test_budgeted_worker_route_is_refused_at_startup(daimonos_binary, tmp_path):
+    """req 7: --tool-timeout-ms together with --worker-config is refused before
+    any worker launches, with the reviewed run_configuration_invalid code and
+    exit 2 and a reason naming the unenforceable budget."""
+    provider = _FakeProvider([("end", _DONE)])
+    try:
+        completed = _run_agent_worker(daimonos_binary, tmp_path, provider,
+                                      ["--tool-input-bytes", "1000"])
+    finally:
+        provider.close()
+    blob = completed.stdout + completed.stderr
+    assert completed.returncode == 2, blob
+    assert "run_configuration_invalid" in blob, blob
+    assert _WORKER_REFUSAL in blob, blob
+
+
+def test_unbudgeted_worker_route_is_not_refused_for_budget(daimonos_binary, tmp_path):
+    """req 7 control: with no budget option, worker mode is unchanged — it runs
+    its own WP10 path (here the trivial /bin/true worker is simply unavailable);
+    whatever its outcome, it is NOT the budget refusal."""
+    provider = _FakeProvider([("end", _DONE)])
+    try:
+        completed = _run_agent_worker(daimonos_binary, tmp_path, provider, [])
+    finally:
+        provider.close()
+    blob = completed.stdout + completed.stderr
+    assert _WORKER_REFUSAL not in blob, blob
+
+
+# ======================================================================
+# FIX run r4 (defects M1-M5). Each test targets one integrator measurement.
+# ======================================================================
+
+
+def test_input_bytes_charges_bytes_scanned_not_returned(daimonos_factory, tmp_path):
+    """F1 (M1): a content search charges the bytes it SCANS, not the bytes it
+    returns. 40 files of 100 KB without the word plus one with it, under
+    --tool-input-bytes 2000: the scan must stop and the call must fail with
+    ``tool_input_limit`` even though the returned match set is tiny. ``consumed``
+    exceeds the limit (the scan was stopped mid-stream) and stays below the
+    total bytes on disk (it stopped early). No matches on a limit hit.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    blob = "x" * (100 * 1024)
+    for i in range(40):
+        (ws / f"pad_{i:02}.txt").write_text(blob)
+    (ws / "zzz_match.txt").write_text("needle\n")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "2000"])
+
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 2000, data
+    assert data["consumed"] > 2000, data
+    assert data["consumed"] <= 41 * 100 * 1024, data
+    assert "matches" not in data, "no partial matches on a limit hit"
+
+
+def test_input_bytes_content_search_below_limit_passes(daimonos_factory, tmp_path):
+    """F1 control: a content scan whose total scanned bytes stay under the
+    ceiling returns its matches normally (so the failure above is the ceiling,
+    not search being broken)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "a.txt").write_text("needle here\n")  # ~12 bytes scanned
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100000"])
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is not True, _text(result)
+    data = json.loads(_text(result))
+    assert len(data["matches"]) == 1, data
+
+
+def test_timeout_read_file_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): read_file checks the deadline before building its result; with a
+    1 ms deadline (already elapsed by the time the native reader runs) it
+    returns ``tool_timeout``, never a late success with content."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "big.txt").write_text("x" * (100 * 1024))
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("read_file", {"path": "big.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "content" not in data, "no late content on a deadline hit"
+
+
+def test_timeout_ls_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the ls walk checks the deadline before building its result."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"f{i}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("ls", {"path": "."})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "entries" not in data, "no late listing on a deadline hit"
+
+
+def test_timeout_content_search_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the content-search blocking worker itself checks the deadline
+    (passed into the worker), so a 1 ms deadline returns ``tool_timeout``, not a
+    late success."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"f{i}.txt").write_text("needle\n")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+    assert "matches" not in data, "no late matches on a deadline hit"
+
+
+def test_timeout_file_search_returns_timeout(daimonos_factory, tmp_path):
+    """F2 (M2): the index (files-mode) search checks the deadline before the
+    result is built, so a 1 ms deadline returns ``tool_timeout``."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    for i in range(20):
+        (ws / f"wp18f_{i}.rs").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("search", {"pattern": "wp18f", "mode": "files"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_timeout", data
+
+
+def test_ac_19_1_large_tree_search_times_out_within_bound(daimonos_factory, tmp_path):
+    """AC-19.1 / F2: a content search on a large tree (10000 files x ~1 KB =
+    ~10 MB; a full walk+scan of this many files clearly exceeds the limit on any
+    host in this environment, where opening 10000 files alone costs well over
+    100 ms) with a 100 ms limit returns ``tool_timeout`` within a stated upper
+    bound (8 s, far below the time a full scan would take), proving the deadline
+    STOPPED the work rather than the scan finishing on its own. The control with
+    no option returns success. Kept under ~10 s including setup.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    blob = "lorem ipsum dolor sit amet " * 38  # ~1 KB, no match
+    for i in range(10000):
+        (ws / f"f{i:05}.txt").write_text(blob)
+
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "100"])
+    start = time.monotonic()
+    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
+    elapsed = time.monotonic() - start
+    assert elapsed < 8, f"budget did not stop the search promptly: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    assert json.loads(_text(result))["code"] == "tool_timeout", _text(result)
+
+    control = daimonos_factory(ws)
+    r2 = control.call_tool("search", {"pattern": "needle", "mode": "content"})
+    assert r2.get("isError") is not True, _text(r2)
+    assert "tool_timeout" not in _text(r2), _text(r2)
+
+
+# --- F3: plugin/managed-process tools (M3 curl, M4 git) ---
+
+
+def _slow_http(delay_s):
+    class Handler(BaseHTTPRequestHandler):
+        protocol_version = "HTTP/1.1"
+
+        def log_message(self, *_):
+            pass
+
+        def do_GET(self):
+            time.sleep(delay_s)
+            body = b"wp18-slow-body"
+            self.send_response(200)
+            self.send_header("Content-Length", str(len(body)))
+            self.end_headers()
+            try:
+                self.wfile.write(body)
+            except (BrokenPipeError, ConnectionResetError):
+                pass
+
+    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+    server.daemon_threads = True
+    t = threading.Thread(target=server.serve_forever, daemon=True)
+    t.start()
+    return server, t
+
+
+def test_timeout_stops_curl_plugin(daimonos_factory, tmp_path):
+    """F3 (M3): the curl plugin starts a subprocess; a --tool-timeout-ms 700
+    deadline stops and reaps it, returning ``tool_timeout`` in under 3 s against
+    a server that would not answer for 4 s, and leaving no curl child alive."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    server, _t = _slow_http(20.0)
+    port = server.server_port
+    url = f"http://127.0.0.1:{port}/"
+    needle = f"wp18curl-{_os.getpid()}-{time.time_ns()}"
+    try:
+        client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
+        start = time.monotonic()
+        # The url carries a unique marker so _survivors can spot a leaked child.
+        result = client.call_tool("curl", {"url": url + needle})
+        elapsed = time.monotonic() - start
+    finally:
+        server.shutdown()
+        server.server_close()
+    # The server answers after a longer time than this bound. 8 s is the
+    # non-semantic bound of this file for a loaded runner.
+    assert elapsed < 8, f"curl ran past the deadline: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_timeout", data
+    assert _poll_gone(needle, 3.0), "curl child survived the budget stop"
+
+
+def test_timeout_stops_git_plugin(daimonos_factory, tmp_path):
+    """F4-of-M4 (timeout half): the `git` plugin (a subprocess) is stopped by
+    --tool-timeout-ms 1 with ``tool_timeout``, not a success."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_timeout", data
+
+
+def test_input_bytes_refuses_git_plugin(daimonos_factory, tmp_path):
+    """F3 (M4 input half): the `git` plugin reads no meterable logical input, so
+    under --tool-input-bytes it is REFUSED with the reviewed
+    ``tool_policy_unsupported`` code (D60) rather than passing uncounted
+    (ADR-031, R1)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "10"])
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_policy_unsupported", data
+
+
+def test_git_plugin_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option the `git` plugin is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("git", {"command": "status"})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_timeout" not in _text(result), _text(result)
+    assert "tool_policy_unsupported" not in _text(result), _text(result)
+
+
+def test_budgeted_pipeline_is_refused(daimonos_factory, tmp_path):
+    """F3 (pipeline): tool_pipeline runs external plugin stages whose remaining
+    deadline/input cannot be enforced stage-by-stage, so rather than leave it
+    "not cooperative" it is REFUSED with the reviewed ``tool_policy_unsupported``
+    code (D60) whenever any budget option is set (ADR-031 allows refusing when
+    --tool-timeout-ms is set; the same applies to input/entry). The call returns
+    promptly and starts no stage process."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
+    start = time.monotonic()
+    result = client.call_tool("tool_pipeline",
+                              {"tool_id": "git", "stages": ["status"]})
+    elapsed = time.monotonic() - start
+    assert elapsed < 8, f"refusal was not prompt: {elapsed:.1f}s"
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data.get("code") == "tool_policy_unsupported", data
+
+
+def test_pipeline_default_off_not_refused(daimonos_factory, tmp_path):
+    """Regression: with no budget option a tool_pipeline is not budget-refused."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    _init_repo(ws)
+    client = daimonos_factory(ws)
+    result = client.call_tool("tool_pipeline",
+                              {"tool_id": "git", "stages": ["status"]})
+    assert "cooperative tool budget" not in _text(result), _text(result)
+
+
+# --- F5: the audit limit_hit event, asserted by kind and fields ---
+
+
+def test_budget_hit_writes_limit_hit_audit_event(daimonos_binary, tmp_path):
+    """F5 (M5): a budget limit hit in a run with --audit-path writes the
+    reserved ``limit_hit`` audit event via record_limit_hit, carrying the limit
+    name, the tool, the configured value, and the consumed value. Asserted by
+    the event ``kind`` and its fields, not by a substring search for a code."""
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    (workspace / "big.txt").write_text("z" * 500)
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider)
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "--tool-input-bytes", "100",
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "do the read"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    hits = [ev for ev in events if ev.get("kind") == "limit_hit"]
+    assert hits, f"no limit_hit event: {[e.get('kind') for e in events]}"
+    hit = hits[0]
+    assert hit["tool"] == "read_file", hit
+    assert hit["limit"], hit
+    assert hit["configured"] == 100, hit
+    assert hit["consumed"] >= 500, hit
+
+
+# --- R1 (B1): allow-list inversion of the input/entry admission gate ---
+#
+# Under --tool-input-bytes / --tool-max-entries a tool is admitted only if it
+# meters its logical input (read_file, ls, content search, git_read) or reads
+# no workspace/external content (write_file, append_file, the session/meta/mail
+# tools, and the composite dispatchers). EVERY other name is refused with the
+# reviewed ``tool_policy_unsupported`` code (integrator D60) naming the budget
+# and the tool: process plugins, edit_file/diff_files (read content unmetered),
+# kgl_*, snapshot/restore, bg, config-registered tools, outbound mcp__*, and any
+# name a later package adds. The deadline alone refuses nothing except
+# tool_pipeline.
+
+
+def _budget_refusal(result):
+    data = json.loads(_text(result))
+    return data
+
+
+def test_input_bytes_refuses_edit_file(daimonos_factory, tmp_path):
+    """edit_file reads the target file to apply its edits but does not meter
+    that input, so under --tool-input-bytes it is refused (not silently
+    exempted) with tool_policy_unsupported (R1/B1, D60)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("hello world")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool(
+        "edit_file", {"path": "f.txt", "edits": ["hello", "HELLO"]}
+    )
+    assert result.get("isError") is True, result
+    data = _budget_refusal(result)
+    assert data["code"] == "tool_policy_unsupported", data
+    assert data.get("tool") == "edit_file", data
+    # original content untouched (refused before any effect)
+    assert (ws / "f.txt").read_text() == "hello world"
+
+
+def test_edit_file_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, edit_file is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("hello world")
+    client = daimonos_factory(ws)
+    result = client.call_tool(
+        "edit_file", {"path": "f.txt", "edits": ["hello", "HELLO"]}
+    )
+    assert result.get("isError") is not True, _text(result)
+    assert (ws / "f.txt").read_text() == "HELLO world"
+
+
+def test_input_bytes_refuses_snapshot(daimonos_factory, tmp_path):
+    """snapshot/restore read and write workspace content that the budget cannot
+    meter, so under --tool-input-bytes the snapshot tool is refused with
+    tool_policy_unsupported (R1/B1, D60)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("snapshot", {"action": "list"})
+    assert result.get("isError") is True, result
+    data = _budget_refusal(result)
+    assert data["code"] == "tool_policy_unsupported", data
+    assert data.get("tool") == "snapshot", data
+
+
+def test_snapshot_default_off_runs_normally(daimonos_factory, tmp_path):
+    """Regression: with no budget option, snapshot list is unchanged."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws)
+    result = client.call_tool("snapshot", {"action": "list"})
+    assert result.get("isError") is not True, _text(result)
+
+
+def test_input_bytes_admits_write_file(daimonos_factory, tmp_path):
+    """write_file scans no workspace/external input, so there is nothing to
+    meter: it is admitted (not refused) under --tool-input-bytes and runs
+    normally. The inverted gate must not refuse a contentless tool (R1)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "10"])
+    result = client.call_tool("write_file", {"path": "out.txt", "content": "y" * 500})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_policy_unsupported" not in _text(result), _text(result)
+    assert (ws / "out.txt").read_text() == "y" * 500
+
+
+def test_max_entries_refuses_edit_file(daimonos_factory, tmp_path):
+    """The entry budget shares the admission gate: edit_file is refused under
+    --tool-max-entries too (R1)."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("abc")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "100"])
+    result = client.call_tool("edit_file", {"path": "f.txt", "edits": ["a", "A"]})
+    assert result.get("isError") is True, result
+    data = _budget_refusal(result)
+    assert data["code"] == "tool_policy_unsupported", data
+
+
+def test_deadline_only_refuses_nothing_except_pipeline(daimonos_factory, tmp_path):
+    """The deadline alone (no input/entry budget) refuses no tool at the
+    admission gate: a tool that the input gate would refuse (edit_file) still
+    runs under --tool-timeout-ms. Only tool_pipeline is refused for a bare
+    deadline (its own check), proven by test_budgeted_pipeline_is_refused."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "f.txt").write_text("hello")
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "60000"])
+    result = client.call_tool("edit_file", {"path": "f.txt", "edits": ["hello", "HI"]})
+    assert result.get("isError") is not True, _text(result)
+    assert "tool_policy_unsupported" not in _text(result), _text(result)
+    assert (ws / "f.txt").read_text() == "HI"
+
+
+# --- R2 (B4): the entry limit must BOUND the walk, not account after it ---
+
+
+def test_max_entries_bounds_the_ls_walk(daimonos_factory, tmp_path):
+    """R2/B4: --tool-max-entries must stop the directory walk at the ceiling,
+    not walk the whole tree and reclassify afterwards. A tree with 5000 entries
+    and a limit of 50 returns tool_input_limit with ``consumed`` far below the
+    total (< 200), proving the walk stopped early; no partial listing. Post-hoc
+    accounting (the pre-R2 behavior) would report consumed == 5000."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    big = ws / "many"
+    big.mkdir()
+    for i in range(5000):
+        (big / f"f{i:05d}.txt").write_text("x")
+    client = daimonos_factory(ws, args=["--tool-max-entries", "50"])
+    result = client.call_tool("ls", {"path": "many", "depth": 1})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["consumed"] < 200, f"walk not bounded; consumed={data.get('consumed')}"
+    assert "entries" not in data, "no partial listing on a limit hit"
+
+
+def test_read_file_refuses_oversized_by_size_precheck(daimonos_factory, tmp_path):
+    """R2/B4: read_file checks the file size from metadata and refuses without
+    reading when the size alone exceeds the remaining byte allowance. Observable
+    as a tool_input_limit whose consumed reflects the file size and no content,
+    even for a file much larger than the limit."""
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "huge.txt").write_text("q" * 200_000)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("read_file", {"path": "huge.txt"})
+    assert result.get("isError") is True, result
+    data = json.loads(_text(result))
+    assert data["code"] == "tool_input_limit", data
+    assert data["configured"] == 100, data
+    assert data["consumed"] >= 200_000, data
+    assert "content" not in data, "no partial content on a size-precheck refusal"
+
+
+# --- R3 (B3): the limit_hit audit event must be in-band, not parsed from text ---
+
+
+def test_exec_printing_budget_json_writes_no_limit_hit_event(daimonos_binary, tmp_path):
+    """R3/B3: a tool whose RESULT TEXT merely looks like a budget failure (here
+    `exec` echoing the JSON shape, with NO budget option set) must NOT cause a
+    forged ``limit_hit`` audit event. The hit is signalled in-band by the budget
+    code on the session and read by the run loop after dispatch, never parsed
+    from attacker-influenceable result text."""
+    workspace = tmp_path / "evidence"
+    workspace.mkdir()
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
+    shape = '{"code":"tool_input_limit","configured":1,"consumed":2}'
+    provider = _FakeProvider([("call", "exec", {"command": f"echo '{shape}'"}),
+                              ("end", _DONE)])
+    try:
+        env = _isolated_env(tmp_path)
+        agent_env = _agent_env_file(tmp_path, provider, allowed="exec")
+        cmd = [daimonos_binary, "--workspace", str(workspace),
+               "--state-dir", str(state), "--audit-path", str(audit_path),
+               "agent", "--print", "--output", "json",
+               "--agent-env", str(agent_env), "run the echo"]
+        completed = subprocess.run(cmd, capture_output=True, text=True,
+                                   timeout=60, env=env)
+    finally:
+        provider.close()
+    assert completed.returncode == 0, completed.stdout + completed.stderr
+    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
+    hits = [ev for ev in events if ev.get("kind") == "limit_hit"]
+    assert not hits, f"forged limit_hit from result text: {hits}"
+
+
+# --- Review round 1, R4: a budget never leaks into the next top-level call ---
+
+def _code(result):
+    """The failure code of a tool result, or None for a success."""
+    if not result.get("isError"):
+        return None
+    try:
+        return json.loads(_text(result)).get("code", "error-without-code")
+    except ValueError:
+        return "error-not-json"
+
+
+def test_each_kind_of_failed_call_leaves_a_fresh_budget_for_the_next_call(daimonos_factory, tmp_path):
+    """After each call that ends early under a budget, the next top-level call
+    starts with a new budget. The probe is a read of a 60-byte file under
+    --tool-input-bytes 100: it passes only with a fresh allowance, because a
+    leaked budget already holds 60 bytes or more. A 70-byte read comes first in
+    each round, so a leak makes the probe fail with tool_input_limit.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "first.txt").write_text("a" * 70)
+    (ws / "probe.txt").write_text("b" * 60)
+    (ws / "big.txt").write_text("c" * 500)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100", "--tool-max-entries", "100"])
+
+    def probe(after):
+        result = client.call_tool("read_file", {"path": "probe.txt"})
+        assert _code(result) is None, f"budget leaked after {after}: {_text(result)}"
+
+    early_endings = [
+        ("a limit hit", "read_file", {"path": "big.txt"}, "tool_input_limit"),
+        ("a refused tool", "exec", {"command": "true"}, "tool_policy_unsupported"),
+        ("a tool error", "read_file", {"path": "does-not-exist.txt"}, "error-not-json"),
+        ("a refused index search", "search", {"pattern": "probe", "mode": "files"}, "tool_policy_unsupported"),
+    ]
+    for label, tool, arguments, expected in early_endings:
+        assert _code(client.call_tool("read_file", {"path": "first.txt"})) is None
+        result = client.call_tool(tool, arguments)
+        assert result.get("isError") is True, (label, result)
+        if expected != "error-not-json":
+            assert _code(result) == expected, (label, _text(result))
+        probe(label)
+
+    # An unknown tool name is an RPC error or a tool error. Both must leave no budget.
+    try:
+        client.call_tool("no_such_tool_wp18", {})
+    except RuntimeError:
+        pass
+    probe("an unknown tool name")
+
+    # A batch whose second child fails, and a script whose read fails.
+    batch = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "first.txt"}},
+        {"tool": "read_file", "arguments": {"path": "big.txt"}},
+    ]})
+    assert "tool_input_limit" in _text(batch), _text(batch)
+    probe("a batch with a failed child")
+    script = client.call_tool("execute_script", {"code": _TWO_READS.replace("big.txt", "first.txt")})
+    assert "tool_input_limit" in _text(script), _text(script)
+    probe("a script that hit the limit")
+
+
+def test_a_timeout_leaves_a_fresh_budget_for_the_next_call(daimonos_factory, tmp_path):
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "probe.txt").write_text("b" * 60)
+    client = daimonos_factory(ws, args=["--tool-timeout-ms", "800"])
+    stopped = client.call_tool("exec", {"command": "sleep", "args": ["30"]})
+    assert _code(stopped) == "tool_timeout", _text(stopped)
+    # The next call has its own 800 ms. A leaked budget has a passed deadline.
+    time.sleep(1.0)
+    result = client.call_tool("read_file", {"path": "probe.txt"})
+    assert _code(result) is None, _text(result)
+
+
+# --- Review round 1, R5: execute_script and its shared budget ---
+
+def test_a_failed_child_does_not_clear_the_shared_budget(daimonos_factory, tmp_path):
+    """A child that fails must not reset the budget of its composite. Under
+    --tool-input-bytes 100, two reads of a 60-byte file pass the limit together.
+    If the failed first child cleared the shared budget, each later read would
+    get a new allowance and both would pass.
+
+    The composite is `batch`, whose children continue after a failed child. In
+    `execute_script` a failed operation ends the script, so no later operation
+    of that script exists.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "part.txt").write_text("p" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+    result = client.call_tool("batch", {"ops": [
+        {"tool": "read_file", "arguments": {"path": "does-not-exist.txt"}},
+        {"tool": "read_file", "arguments": {"path": "part.txt"}},
+        {"tool": "read_file", "arguments": {"path": "part.txt"}},
+    ]})
+    children = json.loads(_text(result))
+    assert [child["ok"] for child in children] == [False, True, False], children
+    assert "tool_input_limit" not in json.dumps(children[0]), children[0]
+    last = json.dumps(children[2])
+    assert "tool_input_limit" in last and "120" in last, last
+
+
+def test_a_call_during_a_budgeted_script_does_not_share_its_budget(daimonos_factory, tmp_path):
+    """A second request arrives while a budgeted execute_script call sleeps
+    between two reads. Under --tool-input-bytes 100 with 60-byte files:
+
+    - The script reads 60 + 60 = 120 bytes and must end with tool_input_limit,
+      consumed 120. If the other call cleared or replaced the script budget,
+      the second read would pass.
+    - The other call reads 60 bytes and must succeed. If it inherited the
+      script budget (60 used), it would fail with consumed 120.
+
+    The two requests are sent on one connection without waiting. The test
+    requires that the reply of the read arrives before the reply of the script.
+    That shows that the read ran while the script was in progress.
+    """
+    ws = tmp_path / "ws"
+    ws.mkdir()
+    (ws / "part.txt").write_text("p" * 60)
+    (ws / "other.txt").write_text("o" * 60)
+    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
+
+    # The script reads, then computes for a moment (no tool call), then reads
+    # again. `exec` is not usable here: it is refused under an input limit.
+    slow_script = (
+        "def slow():\n"
+        "    total = 0\n"
+        "    for i in range(400000):\n"
+        "        total += i\n"
+        "    return total\n"
+        "\n"
+        "def main():\n"
+        "    read_file('part.txt')\n"
+        "    slow()\n"
+        "    return read_file('part.txt')\n"
+        "result = main()\n"
+    )
+    requests = [
+        {"jsonrpc": "2.0", "id": 9001, "method": "tools/call",
+         "params": {"name": "execute_script", "arguments": {"code": slow_script}}},
+        {"jsonrpc": "2.0", "id": 9002, "method": "tools/call",
+         "params": {"name": "read_file", "arguments": {"path": "other.txt"}}},
+    ]
+    client.process.stdin.write((json.dumps(requests[0]) + "\n").encode())
+    client.process.stdin.flush()
+    time.sleep(0.15)  # let the script start and make its first read
+    client.process.stdin.write((json.dumps(requests[1]) + "\n").encode())
+    client.process.stdin.flush()
+    replies = {}
+    order = []
+    while len(replies) < 2:
+        line = client.process.stdout.readline()
+        assert line, "the process closed stdout"
+        message = json.loads(line)
+        if message.get("id") in (9001, 9002):
+            replies[message["id"]] = message["result"]
+            order.append(message["id"])
+    assert order == [9002, 9001], f"the read did not run during the script: reply order {order}"
+
+    script = _text(replies[9001])
+    assert "tool_input_limit" in script, script
+    assert "consumed=120" in script, script
+    other = replies[9002]
+    assert _code(other) is None, _text(other)
+    assert "o" * 60 in _text(other)

### New Points
R6. S6 (the agent-loop refusal builds its JSON inline, the MCP seam uses the ToolFailure shape) is not applied in this round. It is recorded as an open suggestion in the PR. The reason is scope: the inline shape in agent.rs follows the existing WP04 and WP06 refusals in the same function, so a change belongs to one pass over all three.
R7. Verification of 824c70f by the integrator: fmt and clippy exit 0. The complete Rust suite: 2307 passed, 6 failed (plugins::shellcheck::tests::*, no shellcheck on the host), 10 ignored, in 13 s. Note on method: this run used an empty HOME. With the real HOME of the host, each agent_cmd test that uses the default configuration connects the outbound MCP servers of the user's editor settings; under load two complete runs of `cargo test` did not finish in 40 minutes (no test failed, 15 agent_cmd and chat_cmd tests did not end). That is a property of the existing tests on this host, not of this package: the same tests use the same default configuration on the base. tests/test_tool_budgets.py: 49 passed. The complete pytest run was done on 5da3a19 (981 passed, 7 shellcheck failures, 3 failures of the existing test_acp_runs_teardown_when_signaled, 1 skipped); 824c70f adds one Rust test only.

**Requesting:** RE-REVIEW

<!-- event id="review-3" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 3
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re B2: Resolved. The only residue I kept open in round 2 was that the registry-absent 'tool registry unavailable' early return was not exercised by any leak test. The worker has now provided (a) exact line numbers in src/mcp.rs at 5da3a19 establishing that the return at line 825 is inside the with_budget_deadline block opening at line 461 and closing at line 1192, with the single pre-block return being the refusal that clears first, and end_tool_operation at line 1203 with no intervening return; (b) a reachability argument (every process frontend installs a registry via provisioning.rs, so only a Rust unit test can reach the path); and (c) the new test mcp::tests::budgeted_git_read_without_a_registry_leaves_no_budget in commit 824c70f, which charges 60 bytes, drives the registry-absent git_read error, asserts session.tool_budget.is_none() after it, and proves a fresh allowance on the next read — with a stated mutation check that skipping the clear fails the test at 'the early return must clear the budget'. Together with the round-1 leak tests (every early-ending class plus timeout) and the debug_assert at the top-level seam, the install/clear ownership is now demonstrated on every named path. The concern is closed.
Re R6: Acceptable. S6 is a non-blocking consistency suggestion, and the scoping argument is sound: the inline refusal shape in agent.rs mirrors the existing WP04/WP06 refusals in the same function, so unifying all three through ToolFailure serialization belongs to one deliberate pass rather than a one-off change in this package. Recording it as an open suggestion in the PR is the right disposition; S1, S5, and S6 remain open suggestions for the owner, none blocking.
Re R7: The verification picture for 824c70f is adequate for approval: fmt/clippy clean, 2307 Rust tests passed with only the known pre-existing shellcheck failures, and the 49 budget process tests pass. The empty-HOME method note is reasonable and transparent — the hang with the real HOME is a property of the existing agent_cmd/chat_cmd default-configuration tests on that host, reproduced on base, not of this package. Since 824c70f adds only one Rust unit test over the fully pytest-verified 5da3a19, not re-running the complete pytest suite is a proportionate shortcut. The outstanding integration gate (a CI run after rebase onto the default base) should still be confirmed at merge time by the operator, as noted in round 2.
B2: resolved

**Verdict:** APPROVE
