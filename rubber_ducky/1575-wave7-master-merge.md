# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="wave7-master-merge-1575-dfa20e5" -->

<!-- event id="request" artifact path="1575-wave7-master-merge/artifacts/round-1-review-request.diff" sha256="42e6432b9a0f8f05b7ff1621617887665165a9dfe23b424d946bff9f353ab13b" -->
## Review Request — Round 1
**Task:** 1575 — dbunk combined tree: merge of master (v0.1.6, ADR-036 one-shot sessions) into dbunk/integration-wave7, commit dfa20e5
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Review the conflict resolution of merge commit dfa20e5 (parents: 102d7e2 = dbunk/integration-wave7 with reviewer-runtime packages WP04 to WP11 and WP13 to WP17; f937035 = master with ADR-036 one-shot sessions, --output json, the 0.1.6 release, and release on merge). The diff below is `git show --remerge-diff dfa20e5`: it shows only what the committed merge changed against git's automatic merge, so removed conflict markers and each manual edit are visible. Both parents were reviewed before; the content that merged without conflict is not in the diff. 6 files had conflicts (CHANGELOG.md, src/acp_cmd.rs, src/agent.rs, src/agent_cmd.rs, src/agent_runtime.rs, src/cli.rs), 13 hunks. 9 hunks had an empty base (both sides added text at one position) and keep both sides. 4 hunks needed a manual resolution: (1) agent_cmd.rs, before the AgentConfig: ours added the final-output tool, the capability probe, and its early return; theirs replaced `let model = ...` by the saved-session model, thinking, and system. Resolution: ours without its last line, then theirs. So the session tool check (check_tools) runs after the final-output tool was pushed; this is unreachable today because sessions conflict with --final-output. (2) agent_cmd.rs, the assistant-text echo: ours wraps it in `if !final_output_active`; theirs skips the messages before this invocation on a resume. Resolution: both. (3) agent_runtime.rs, provider construction: ours builds the provider from a controlled profile (WP07) or the legacy path; theirs added explicit_model. Resolution: ours, plus explicit_model, plus a new report_provider (the provider_label of the profile, else the agent env provider) for the JSON document. (4) agent_runtime.rs, the run call: ours buffers stdout when --audit-path is set and writes the run_end audit event before the flush (WP08), then maps worker, probe, and final-output outcomes to exit codes; theirs built RunOptions and called run_agent_with. Resolution: theirs' setup, then ours with run_agent_with in both branches. New in the merge, not from either parent: clap conflicts. --session-id and --resume conflict with --final-output, --model-profile, and --worker-config. --output json conflicts with --final-output. Reason: these combinations were built by neither side (ADR-036 Decision 14, the envelope with a final_output field, is not built; the prefix record does not hold the endpoint of a controlled profile), and both --output json and --final-output own stdout. docs/runtime-modes.md lists the combinations. Verification by the worker on dfa20e5: cargo fmt --check and clippy --all-targets -D warnings exit 0. cargo test --no-fail-fast: 2304 passed, 6 failed, 10 ignored; the 6 are plugins::shellcheck::tests::* (the host has no shellcheck). pytest tests: 924 passed, 7 failed, 1 skipped, before the two new tests; the 7 are tests/test_shellcheck.py. A test-inventory comparison against both parents and the merge base found no lost test (837 Python test functions; Rust test attributes equal ours+theirs-base in each file except src/cli.rs, which has the one new test). Two new process tests in tests/test_agent_oneshot_sessions.py (21 pass): the JSON document names the provider label of a controlled profile (a mutation back to the agent env provider fails it), and the refused combinations exit with code 2 and no stdout. CI on dfa20e5 was running at the time of this request.

### Relevant Code / Diff
diff --git a/CHANGELOG.md b/CHANGELOG.md
remerge CONFLICT (content): Merge conflict in CHANGELOG.md
index da698f6..6a4bb13 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -7,7 +7,6 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ## [Unreleased]
 
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
 ### Added
 
 - **`--capability-probe-payload` (FR-04).** A default-off `agent --print`
@@ -263,7 +262,7 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
   is never read or walked through, including intermediate directory links and
   links named as a tool/walk scope. Combines with `--confine-paths`; default off
   leaves behavior unchanged.
-=======
+
 ### Changed
 
 - **Releases start from CI.** A merge to `master` that changes the version in
@@ -329,7 +328,6 @@ This release has the 13 PRs that merged after v0.1.5 (#252 to #263, and #284).
 
 - Accepted ADRs 021 to 032 and the reviewer-runtime glossary (#256).
 - Vikunja project URLs in `AGENTS.md` (#263).
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
 
 ## [0.1.5] - 2026-10-18
 
diff --git a/docs/runtime-modes.md b/docs/runtime-modes.md
index a35b249..d0c9c70 100644
--- a/docs/runtime-modes.md
+++ b/docs/runtime-modes.md
@@ -52,7 +52,19 @@ daimonos agent --resume run-42 --output json "The tests failed: <output>"
 | `--output json` | Print one JSON result document on stdout. |
 
 The three options conflict with `--interactive` and `--dry-run`. `--session-id`
-and `--resume` conflict with each other. The top-level `--session-id` (before
+and `--resume` conflict with each other.
+
+Combinations with the reviewer-runtime options:
+
+- `--session-id` and `--resume` are refused with `--final-output`,
+  `--model-profile`, and `--worker-config`. These combinations are not built.
+  The session record does not hold the endpoint of a model profile.
+- `--output json` is refused with `--final-output`. Both options own stdout.
+  ADR-036 Decision 14 defines the combination. It is not built.
+- `--output json` works with `--model-profile`. The field `provider` is then
+  the `provider_label` of the profile.
+- `--output json` with `--worker-config` or `--audit-path` is accepted. No test
+  covers these two combinations. The top-level `--session-id` (before
 `agent`) is a different option: it filters `--stats`.
 
 **Sessions**
diff --git a/src/acp_cmd.rs b/src/acp_cmd.rs
remerge CONFLICT (content): Merge conflict in src/acp_cmd.rs
index 44815fd..cb36020 100644
--- a/src/acp_cmd.rs
+++ b/src/acp_cmd.rs
@@ -1714,14 +1714,11 @@ fn build_agent_config(
             failover_models,
             ..crate::agent::ProviderRetryConfig::default()
         },
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
         // WP08: --audit-path currently wires into the one-shot agent frontend
         // only; ACP-session auditing is a future integrator hook.
         audit: None,
-=======
         // ACP sessions do not mark a stable message prefix (ADR-036 is one-shot only).
         stable_prefix: false,
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
         before_tool_call: Some(build_before_tool_call_hook(
             Arc::clone(&connection),
             session_id.clone(),
diff --git a/src/agent.rs b/src/agent.rs
remerge CONFLICT (content): Merge conflict in src/agent.rs
index 76532a3..ad580f8 100644
--- a/src/agent.rs
+++ b/src/agent.rs
@@ -240,7 +240,6 @@ pub struct AgentConfig {
     pub on_provider_notice: Option<ProviderNoticeHook>,
     /// Bounded retry for *transient* provider failures (vikunja #1240).
     pub provider_retry: ProviderRetryConfig,
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
     /// WP08 (FR-04, ADR-026): caller-requested local run audit sink. `None` =
     /// audit off. When present, every provider response and tool dispatch in
     /// this loop is recorded; a required-audit write failure poisons the sink
@@ -270,12 +269,10 @@ pub struct AgentConfig {
     /// (`final_output`) and is never forwarded to a worker. `None` leaves
     /// dispatch unchanged (legacy ACP bridging stays fail-open).
     pub worker_executor: Option<std::sync::Arc<crate::worker_runtime::WorkerExecutor>>,
-=======
     /// Mark the whole message list as the stable prefix on each request, so
     /// the provider adapter can place its cache breakpoint at the end of the
     /// conversation (ADR-036 Decision 7). `false` sends `stable_prefix_len: 0`.
     pub stable_prefix: bool,
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
 }
 
 /// How hard to retry a provider failure the adapter classified as transient
diff --git a/src/agent_cmd.rs b/src/agent_cmd.rs
remerge CONFLICT (content): Merge conflict in src/agent_cmd.rs
index fde622c..7a1c5c1 100644
--- a/src/agent_cmd.rs
+++ b/src/agent_cmd.rs
@@ -267,13 +267,6 @@ pub async fn run_agent(
     args: AgentCmdArgs,
     out: &mut dyn Write,
 ) -> Result<AgentResult> {
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
-    // FR-02 (WP06, ADR-023) R1: the explicit allowlist filters the offered
-    // catalog at its single source (`active_schemas_for_policy`), so the
-    // per-frontend `retain` below is gone. With no explicit list this is a
-    // no-op (legacy behavior).
-    let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
-=======
     run_agent_with(provider, workspace, cfg, args, RunOptions::default(), out).await
 }
 
@@ -289,7 +282,11 @@ pub async fn run_agent_with(
     out: &mut dyn Write,
 ) -> Result<AgentResult> {
     let started = std::time::Instant::now();
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
+    // FR-02 (WP06, ADR-023) R1: the explicit allowlist filters the offered
+    // catalog at its single source (`active_schemas_for_policy`), so the
+    // per-frontend `retain` below is gone. With no explicit list this is a
+    // no-op (legacy behavior).
+    let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
     let schemas = tool_facade::active_schemas_for_policy(
         workspace,
         &cfg.prompts.resolved_tool_descriptions,
@@ -444,7 +441,6 @@ pub async fn run_agent_with(
         tools.extend(mcp.tools().into_iter().filter(|t| policy.allows(&t.name)));
     }
 
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
     // WP05 (ADR-024): offer the caller's final-answer tool with its schema
     // unchanged and switch the loop to terminal-call accounting. The tool is
     // intercepted model-side and never dispatched or routed to a worker.
@@ -503,8 +499,6 @@ pub async fn run_agent_with(
             });
         }
     }
-    let model = args.model.unwrap_or_else(|| "claude-opus-4-8".to_string());
-=======
     // ADR-036 Decision 5: a resumed session keeps its request prefix. It runs
     // on the saved model and thinking level with the saved system prompt, and
     // a different tool list is refused before any provider call.
@@ -526,7 +520,6 @@ pub async fn run_agent_with(
         Some(saved) => saved.prefix.system.clone(),
         None => crate::prompts::agent_system_for_workspace(&cfg, Some(workspace)).await,
     };
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
     let before_tool_call = args.safety.map(|p| p.into_before_hook());
     // #1575 (invariant 4 / R6): a thought-log open failure here is a post-probe
     // early error; reap the owned worker through its managed lifecycle before
@@ -572,7 +565,10 @@ pub async fn run_agent_with(
             label: "agent".to_string(),
         }),
         remote_tool_dispatch: agent_mcp.as_ref().map(|mcp| mcp.dispatch_hook()),
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
+        // ADR-036 Decision 7. Only with prompt caching on: the Anthropic
+        // adapter sets a cache marker for any non-zero prefix length, and the
+        // mark must have no effect while prompt caching is off.
+        stable_prefix: options.session.is_some() && options.prompt_cache,
         audit: args.audit.clone(),
         // WP05 (ADR-024): final-output mode makes each generation exactly one
         // HTTP attempt so the inclusive model-request ceiling is exact. Disable
@@ -600,12 +596,6 @@ pub async fn run_agent_with(
             0
         },
         worker_executor: worker_executor.clone(),
-=======
-        // ADR-036 Decision 7. Only with prompt caching on: the Anthropic
-        // adapter sets a cache marker for any non-zero prefix length, and the
-        // mark must have no effect while prompt caching is off.
-        stable_prefix: options.session.is_some() && options.prompt_cache,
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
         ..AgentConfig::default()
     };
 
@@ -771,18 +761,6 @@ pub async fn run_agent_with(
     // completed provider run has closed its span and reached analytics.
     thought_capture_result?;
 
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
-    // WP05 (ADR-024): final-output mode owns stdout — only the validated
-    // argument JSON (emitted by the caller) may appear there. Suppress the
-    // normal assistant-text echo so nothing else reaches stdout (AC-01.7).
-    if !final_output_active {
-        for msg in &result.messages {
-            if matches!(msg.role, Role::Assistant) {
-                for block in &msg.content {
-                    if let ContentBlock::Text(t) = block {
-                        writeln!(out, "{t}")?;
-                    }
-=======
     // Decision 15: a resumed run prints only the assistant text that this
     // invocation added. Every other run prints as before.
     let already_printed = if options.session.as_ref().is_some_and(|s| s.is_resume()) {
@@ -790,12 +768,16 @@ pub async fn run_agent_with(
     } else {
         0
     };
-    for msg in result.messages.iter().skip(already_printed) {
-        if matches!(msg.role, Role::Assistant) {
-            for block in &msg.content {
-                if let ContentBlock::Text(t) = block {
-                    writeln!(out, "{t}")?;
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
+    // WP05 (ADR-024): final-output mode owns stdout — only the validated
+    // argument JSON (emitted by the caller) may appear there. Suppress the
+    // normal assistant-text echo so nothing else reaches stdout (AC-01.7).
+    if !final_output_active {
+        for msg in result.messages.iter().skip(already_printed) {
+            if matches!(msg.role, Role::Assistant) {
+                for block in &msg.content {
+                    if let ContentBlock::Text(t) = block {
+                        writeln!(out, "{t}")?;
+                    }
                 }
             }
         }
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
remerge CONFLICT (content): Merge conflict in src/agent_runtime.rs
index 1d485ab..3a637af 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -322,11 +322,13 @@ pub async fn run_agent(
         agent_env,
         debug_thoughts,
         debug_thoughts_path,
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
         model_profile,
         final_output,
         capability_probe_payload,
         worker_config,
+        output,
+        oneshot_session_id,
+        resume,
     } = args;
     // WP09 (FR-04 reqs 14-17, ADR-027): the capability probe requires FR-01
     // structured final output AND a complete controlled model profile (PM
@@ -356,11 +358,6 @@ pub async fn run_agent(
         Some(path) => Some(load_controlled_profile(path)?),
         None => None,
     };
-=======
-        output,
-        oneshot_session_id,
-        resume,
-    } = args;
     let session_request = oneshot_session_id
         .map(crate::oneshot_session::SessionRequest::Start)
         .or(resume.map(crate::oneshot_session::SessionRequest::Resume));
@@ -368,7 +365,6 @@ pub async fn run_agent(
         // ADR-036: a bad id is refused before any store or provider access.
         crate::oneshot_session::validate_id(request.id())?;
     }
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
     let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
     // Pre-resolution intent capture: the operator asked for interactive but
     // no TTY is attached. Only the Print fallback's require_agent_task error
@@ -706,7 +702,15 @@ pub async fn run_agent(
     let agent = load_agent_env(agent_env, &cfg)?;
     let cfg = with_skill_dir(cfg, &agent)?;
     let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
+    // An explicit --model. The model of the agent env file is a default, not
+    // a request, so a resume does not compare it with the saved model.
+    let explicit_model = model.clone();
+    // ADR-036 with WP07: under a controlled profile, the JSON result document
+    // names the provider label of the profile, not the agent env provider.
+    let report_provider = controlled_profile
+        .as_ref()
+        .map(|profile| profile.spec.provider_label.clone())
+        .unwrap_or_else(|| effective_provider.clone());
     // WP10 (R1, ADR-028): capture the resolved model credential BEFORE the
     // provider consumes it, so a worker whose explicit env value or argv entry
     // discloses it can be refused before any worker launches. Default provider:
@@ -735,17 +739,6 @@ pub async fn run_agent(
                 )?,
             )
         };
-=======
-    // An explicit --model. The model of the agent env file is a default, not
-    // a request, so a resume does not compare it with the saved model.
-    let explicit_model = model.clone();
-    let effective_model = model.unwrap_or_else(|| agent.model.clone());
-    let llm = build_provider(
-        &effective_provider,
-        &agent,
-        providers::ProviderTimeouts::from_config(&cfg.provider_timeouts),
-    )?;
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
     let analytics_store = if cfg.analytics.enabled {
         let db_path = cfg.analytics.resolved_db_path();
         match analytics::AnalyticsStore::open_async(&db_path, cfg.analytics.retention_days).await {
@@ -819,7 +812,31 @@ pub async fn run_agent(
         capability_probe_sink: capability_probe_sink.clone(),
         worker_config: worker_config_runtime.clone(),
     };
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
+    // The endpoints that `build_provider` uses: the primary base URL and each
+    // secondary provider that is not the selected provider.
+    let endpoint = crate::oneshot_session::endpoint_digest(
+        &agent.base_url,
+        agent
+            .secondary_providers
+            .iter()
+            .filter(|secondary| secondary.name != effective_provider)
+            .map(|secondary| (secondary.name.as_str(), secondary.base_url.as_str())),
+    );
+    let session = match session_request {
+        Some(request) => {
+            let session = open_oneshot_session(&cfg, request)?;
+            session.check_overrides(explicit_model.as_deref(), &effective_provider, &endpoint)?;
+            Some(session)
+        }
+        None => None,
+    };
+    let options = agent_cmd::RunOptions {
+        provider: report_provider,
+        output,
+        session,
+        prompt_cache: agent.prompt_cache,
+        endpoint,
+    };
     // WP08 (R2, ADR-026): the terminal `run_end` audit event must be written —
     // and any required-audit write failure surfaced as exit 23 — BEFORE the
     // run's assistant output reaches stdout, so a finish-time audit failure can
@@ -830,7 +847,9 @@ pub async fn run_agent(
     let result = if let Some(audit) = &audit {
         use std::io::Write as _;
         let mut buffer: Vec<u8> = Vec::new();
-        let result = agent_cmd::run_agent(llm.as_ref(), workspace, cfg, args, &mut buffer).await;
+        let result =
+            agent_cmd::run_agent_with(llm.as_ref(), workspace, cfg, args, options, &mut buffer)
+                .await;
         // WP09 (G1): peek (not take) the probe verdict so the terminal audit
         // record agrees with the process exit that `emit_probe_outcome` will
         // produce below; `emit_probe_outcome` still consumes the sink after.
@@ -854,7 +873,15 @@ pub async fn run_agent(
         stdout.flush()?;
         result
     } else {
-        agent_cmd::run_agent(llm.as_ref(), workspace, cfg, args, &mut std::io::stdout()).await
+        agent_cmd::run_agent_with(
+            llm.as_ref(),
+            workspace,
+            cfg,
+            args,
+            options,
+            &mut std::io::stdout(),
+        )
+        .await
     };
     // WP10 (FR-05, ADR-028): a required worker failure (initialization, call,
     // liveness, or teardown) ends the run with the reviewed `worker_unavailable`
@@ -1077,42 +1104,6 @@ fn finalize_run_audit(
         eprintln!("{}", failure.diagnostic_json());
         std::process::exit(failure.exit_code());
     }
-=======
-    // The endpoints that `build_provider` uses: the primary base URL and each
-    // secondary provider that is not the selected provider.
-    let endpoint = crate::oneshot_session::endpoint_digest(
-        &agent.base_url,
-        agent
-            .secondary_providers
-            .iter()
-            .filter(|secondary| secondary.name != effective_provider)
-            .map(|secondary| (secondary.name.as_str(), secondary.base_url.as_str())),
-    );
-    let session = match session_request {
-        Some(request) => {
-            let session = open_oneshot_session(&cfg, request)?;
-            session.check_overrides(explicit_model.as_deref(), &effective_provider, &endpoint)?;
-            Some(session)
-        }
-        None => None,
-    };
-    let options = agent_cmd::RunOptions {
-        provider: effective_provider,
-        output,
-        session,
-        prompt_cache: agent.prompt_cache,
-        endpoint,
-    };
-    let result = agent_cmd::run_agent_with(
-        llm.as_ref(),
-        workspace,
-        cfg,
-        args,
-        options,
-        &mut std::io::stdout(),
-    )
-    .await?;
-    check_agent_result(&result)
 }
 
 /// Claim a named one-shot session in its own store (ADR-036 Decision 2):
@@ -1137,7 +1128,6 @@ fn open_oneshot_session(
             cfg.session.session_store_busy_timeout_ms,
         ));
     crate::oneshot_session::open(store, request)
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
 }
 
 pub async fn run_chat(
diff --git a/src/cli.rs b/src/cli.rs
remerge CONFLICT (content): Merge conflict in src/cli.rs
index 1334121..3433ab0 100644
--- a/src/cli.rs
+++ b/src/cli.rs
@@ -54,7 +54,6 @@ pub struct AgentArgs {
         conflicts_with = "interactive"
     )]
     pub debug_thoughts_path: Option<PathBuf>,
-<<<<<<< 102d7e2 (merge(#1575): integrate WP14 (no-follow symlink policy) into dbunk/integration-wave7)
     /// WP07 (FR-04): opt-in controlled model transport. Path to ONE JSON object
     /// owning the five required values (`provider_label`, `model`, `base_url`,
     /// `wire` = "chat"|"messages", `credential_env`) and the optional controlled
@@ -113,14 +112,16 @@ pub struct AgentArgs {
     /// and session-daemon frontends never carry this agent-only flag at all.
     #[arg(long, value_name = "PATH", conflicts_with_all = ["interactive", "dry_run"])]
     pub worker_config: Option<PathBuf>,
-=======
     /// One-shot stdout format. `json` prints one result document for each run
     /// whose loop returned a result, including failed runs (ADR-036).
     #[arg(
         long,
         value_enum,
         default_value_t = AgentOutputFormat::Text,
-        conflicts_with_all = ["interactive", "dry_run"]
+        // `final_output`: ADR-036 Decision 14 (the document carries the
+        // validated final output) is not built. Until then the combination
+        // is refused, because both options own stdout.
+        conflicts_with_all = ["interactive", "dry_run", "final_output"]
     )]
     pub output: AgentOutputFormat,
     /// Start a named one-shot session and save it when the run ends. A later
@@ -129,7 +130,16 @@ pub struct AgentArgs {
     #[arg(
         long = "session-id",
         value_name = "ID",
-        conflicts_with_all = ["interactive", "dry_run"]
+        // A session with structured final output, a controlled model profile,
+        // or workers is not built or tested: the prefix record does not hold
+        // the endpoint of a profile. The combination is refused.
+        conflicts_with_all = [
+            "interactive",
+            "dry_run",
+            "final_output",
+            "model_profile",
+            "worker_config"
+        ]
     )]
     pub oneshot_session_id: Option<String>,
     /// Continue a named one-shot session with the task as the new input. The
@@ -138,10 +148,16 @@ pub struct AgentArgs {
     #[arg(
         long,
         value_name = "ID",
-        conflicts_with_all = ["interactive", "dry_run", "oneshot_session_id"]
+        conflicts_with_all = [
+            "interactive",
+            "dry_run",
+            "oneshot_session_id",
+            "final_output",
+            "model_profile",
+            "worker_config"
+        ]
     )]
     pub resume: Option<String>,
->>>>>>> f937035 (Merge pull request #286 from beardfaceguy/ci/1622-release-on-merge)
 }
 
 #[derive(Debug, Args)]
@@ -842,4 +858,34 @@ mod tests {
             );
         }
     }
+
+    /// The one-shot session options and `--output json` (ADR-036) are refused
+    /// with the reviewer-runtime options that they were not built with.
+    #[test]
+    fn agent_session_and_json_options_conflict_with_unbuilt_combinations() {
+        let refused = |extra: &[&str]| {
+            let mut argv = vec!["daimonos", "agent"];
+            argv.extend_from_slice(extra);
+            argv.push("task");
+            Cli::try_parse_from(argv).is_err()
+        };
+        for session in [["--session-id", "s1"], ["--resume", "s1"]] {
+            for other in [
+                ["--final-output", "f.json"],
+                ["--model-profile", "p.json"],
+                ["--worker-config", "w.json"],
+            ] {
+                assert!(
+                    refused(&[session[0], session[1], other[0], other[1]]),
+                    "{session:?} with {other:?}"
+                );
+            }
+        }
+        assert!(refused(&["--output", "json", "--final-output", "f.json"]));
+        // Combinations that stay available.
+        assert!(!refused(&["--final-output", "f.json"]));
+        assert!(!refused(&["--output", "json", "--model-profile", "p.json"]));
+        assert!(!refused(&["--output", "json", "--worker-config", "w.json"]));
+        assert!(!refused(&["--session-id", "s1", "--output", "json"]));
+    }
 }
diff --git a/tests/test_agent_oneshot_sessions.py b/tests/test_agent_oneshot_sessions.py
index 318bd2e..27625a1 100644
--- a/tests/test_agent_oneshot_sessions.py
+++ b/tests/test_agent_oneshot_sessions.py
@@ -536,3 +536,38 @@ def test_only_an_error_stop_reason_exits_non_zero(daimonos_binary, tmp_path, fak
             assert done.returncode == exit_code, (stop_reason, json_output, done.stderr)
             if json_output:
                 assert only_json_document(done.stdout)["stop_reason"] == stop_reason
+
+
+# --- combined tree: ADR-036 options with the reviewer-runtime options ---
+
+def test_output_json_names_the_provider_label_of_a_controlled_profile(daimonos_binary, tmp_path, fake_provider):
+    provider = fake_provider([text_reply("FROM THE PROFILE")])
+    harness = Harness(daimonos_binary, tmp_path, 1)  # the agent env endpoint is not used
+    profile = tmp_path / "profile.json"
+    profile.write_text(json.dumps({
+        "provider_label": "profile-label", "model": "profile/model",
+        "base_url": f"http://127.0.0.1:{provider.port}/v1", "wire": "chat",
+        "credential_env": "PROFILE_TEST_KEY"}))
+    done = subprocess.run(
+        harness.command("--model-profile", str(profile), "--output", "json", "task"),
+        cwd=harness.workspace, env={**harness.environment(), "PROFILE_TEST_KEY": "test-key"},
+        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["provider"], doc["model"]) == ("profile-label", "profile/model")
+    assert doc["final_text"] == "FROM THE PROFILE"
+    assert len(provider.requests) == 1
+
+
+def test_session_options_are_refused_with_the_reviewer_runtime_options(daimonos_binary, tmp_path, fake_provider):
+    provider = fake_provider([text_reply("NEVER SENT")])
+    harness = Harness(daimonos_binary, tmp_path, provider.port)
+    for session in (["--session-id", "run-1"], ["--resume", "run-1"]):
+        for other in (["--final-output", "f.json"], ["--model-profile", "p.json"], ["--worker-config", "w.json"]):
+            done = harness.agent(*session, *other, "task")
+            assert done.returncode == 2 and done.stdout == "", (session, other, done.stderr)
+            assert "cannot be used with" in done.stderr
+    done = harness.agent("--output", "json", "--final-output", "f.json", "task")
+    assert done.returncode == 2 and done.stdout == ""
+    assert provider.requests == []
+    assert not (harness.home / ".daimonos" / "oneshot-sessions").exists()

### Known Concerns
1. With --audit-path, the session save and the JSON document are produced inside run_agent_with while stdout is buffered; the buffer is flushed after the run_end audit event. If run_agent_with returns an error (a failed session save, a thought-log failure), the code reaches `let result = result?;` after the flush. It is not tested that the buffered JSON document is still flushed on that path, or how exit_on_config_invalid and exit_on_worker_failure treat an Err result.
2. --output json with --worker-config, --audit-path, --capability-probe-payload, or --tool-allowlist is accepted and has no test. For example, a failed capability probe returns early from run_agent_with before the JSON document code, so that run prints no document.
3. The capability-probe early return and the allowlist-refusal early return of run_agent_with happen before the session save. This is unreachable for sessions with a probe (the probe requires --final-output, which conflicts with sessions), but a session with --tool-allowlist is allowed: it is not analysed whether an allowlist refusal after a session was claimed leaves the session unsaved with a consumed id.
4. The endpoint digest of a session uses the agent env base URL. With --tool-allowlist or --workspace-read-only the tool list changes, and the tool digest covers that, but no test runs a session with these options.
5. The CHANGELOG conflict was resolved by keeping the dbunk entries under Unreleased and the master sections below. The result has two `### Added` / `### Changed` arrangements under Unreleased that were not re-sorted.
6. The session daemon launch identity (session_bootstrap.rs) did not get the new one-shot options; they conflict with --interactive at the CLI.

### Specific Questions for Reviewer
1. Does any manual resolution drop behavior of one parent, or change the order of the WP08 audit finalization, the exit-code mapping (worker 22, probe 21, final-output), or the ADR-036 save and document order?
2. Are the new clap conflicts the right set: is a combination refused that should work, or allowed that is unsafe or incoherent?
3. Is `report_provider` correct, and is there another field of the JSON document or of the session prefix that is wrong under a controlled profile?
4. Is there an early-return path of the merged run_agent_with that skips the worker shutdown, the MCP shutdown, the session save, or the JSON document where the parents' rules require them?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Audit-path error flush is unverified (worker's own known concern 1). In the merged agent_runtime.rs, when --audit-path is set, run_agent_with writes the session save and (with --output json) the JSON result document into the stdout buffer. If run_agent_with returns Err after those writes (e.g. a failed session save, thought-log failure, or a post-run error), the code path through finalize_run_audit, the flush, and `let result = result?` must still (a) flush whatever was buffered that is contractually owed to stdout, and (b) route Err through exit_on_config_invalid / exit_on_worker_failure consistently with the pre-merge parents. Neither behavior has a test, and this interaction exists only in the merged tree — neither parent had both the WP08 buffer and run_agent_with's session/JSON writes. Add a process test that forces an Err after buffered output (e.g. a session save failure with --audit-path) and asserts the stdout/exit-code contract; or document and assert the intended behavior in a unit test.
B2. --output json combined with the reviewer-runtime options it still accepts is untested and at least one combination is incoherent (worker's known concern 2). A failed capability probe returns early from run_agent_with before the JSON-document code, so `--output json --capability-probe-payload ...` can exit without printing the one result document that `--output json` promises ('prints one result document for each run whose loop returned a result, including failed runs'). Either (a) make the probe path emit the document / define that an early probe exit prints none and test it, or (b) add the probe flag to the clap conflicts like the other unbuilt combinations. Likewise `--output json` with --worker-config and --audit-path are accepted, advertised in docs/runtime-modes.md as untested, and need at least one test each (worker-failure exit 22 with json output; audit buffering with json output) before the merge can claim the combination works.
B3. Session claim vs. early returns in run_agent_with (worker's known concern 3, question 4). A --session-id run with --tool-allowlist is allowed by the new clap rules, but the allowlist-refusal early return happens before the session save. The session was already claimed in open_oneshot_session (agent_runtime.rs, before run_agent_with is called), so an allowlist refusal can leave a claimed/consumed session id with no saved session. The same question applies to any pre-save early return reachable with sessions (e.g. thought-log open failure). This must be analysed and either: the claim must be released/rolled back on pre-save early returns, sessions+--tool-allowlist must be added to the clap conflicts, or the behavior (id consumed, retry semantics) must be specified and tested. Currently there is no test for any session run with --tool-allowlist or --workspace-read-only (concern 4) even though the tool digest is supposed to cover tool-list changes.

### Non-Blocking Suggestions
S1. CHANGELOG.md: under Unreleased, re-sort the merged entries into a single `### Added` and a single `### Changed` section instead of the two arrangements left by the resolution (worker's known concern 5). Low risk, but it is a visible artifact of the merge.
S2. agent_cmd.rs resolution (1) leaves the saved-session tool check (check_tools) running after the final-output tool is pushed. Today this is unreachable because sessions conflict with --final-output, but the ordering is a latent trap: if the clap conflict is ever relaxed (ADR-036 Decision 14 is planned), the saved tool digest would silently include/exclude final_output depending on code order. Add a comment at the check_tools call (or a debug_assert that final-output is inactive when a session exists) so the invariant is explicit.
S3. docs/runtime-modes.md now ships the sentence '--output json with --worker-config or --audit-path is accepted. No test covers these two combinations.' Shipping 'no test covers this' in user-facing docs is odd; once B2 adds the tests, drop that sentence. Also consider documenting --capability-probe-payload in the same combinations table, since its interaction with --output json is the least obvious.
S4. The session daemon launch identity (session_bootstrap.rs) did not receive the new one-shot options (worker's known concern 6). Since they conflict with --interactive at the CLI this is currently safe, but add a short note (code comment or docs) stating that one-shot sessions are intentionally not forwarded to the daemon frontend, so a future wiring change does not drop it silently.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1575-wave7-master-merge/artifacts/round-1-rebuttal.diff" sha256="624ebc087612931efff1f7b12b3932ef2e7e26886ca61212718e5afb9ae66a62" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — Accepted: the path existed only in the merged tree and had no test. Measured with the binary first, then fixed in tests. Result: no code change was needed. Path: with --audit-path, run_agent_with writes the JSON document into the buffer and then returns Err for a failed session save. finalize_run_audit gets the Err, run_terminal_status maps Err to ('error', 1), the buffer is written to stdout and flushed, exit_on_config_invalid and exit_on_worker_failure do nothing for this Err (the first reads only Ok results; the second downcasts only WorkerInitError), and `result?` returns the error, so the process exits 1. New process test test_audit_with_a_failed_session_save_still_prints_the_document (tests/test_oneshot_reviewer_combinations.py): the session store is removed while the provider reply is held; it asserts exactly one JSON document on stdout with session_saved false, exit code 1, the message on stderr, and one run_end event with status error and exit_code 1. A mutation that writes the buffer only for an Ok result fails this test. Two more tests fix the same order for the normal paths: test_audit_with_output_json_prints_one_document_and_a_matching_run_end (end_turn/ok/0 and provider error/error/1) and test_audit_with_a_session_saves_and_resumes.
Re B2: ACCEPT — Accepted, with one correction of the premise: `--output json --capability-probe-payload` was never able to run. A probe requires --final-output (refused at startup with capability_probe_requires_final_output), and --output json conflicts with --final-output. Measured: exit 2 and no stdout for both forms. To make the rule explicit and independent of that chain, option (b) is applied: --output json now conflicts with --capability-probe-payload by name (clap), with a line in the cli test and in docs/runtime-modes.md. For the accepted combinations, tests now exist: test_output_json_with_a_lost_worker_prints_the_document_and_exits_22 (a worker crashes during the run, with and without --audit-path: one document with stop_reason error, exit 22, exactly one worker_unavailable JSON on stderr, no later tool call, run_end worker_unavailable/22), test_output_json_with_a_worker_prints_the_document_of_a_successful_run, and test_output_json_prints_no_document_when_a_worker_run_does_not_start (a worker that cannot start: exit 22 and empty stdout; an allowlist entry that no worker serves: exit 2 and empty stdout). The last test defines the rule that the reviewer asked for: these two cases end before the agent loop, so by the ADR-036 rule ('for each run whose loop returned a result') no document is printed. docs/runtime-modes.md states it, and the sentence 'No test covers these two combinations' is removed (S3).
Re B3: ACCEPT — Accepted as an analysis and test gap. The feared defect does not exist: a claim does not consume an id. open_oneshot_session takes the ADR-033 owner lock and, for a new id, checks that no record exists. It writes no record. The record is written only by the save after the loop. So each early return before the loop (allowlist refusal with a worker, worker init failure, thought-log open failure, skill activation failure, a failed tool-list check) drops the OpenSession, releases the lock, and leaves a new id free and a resumed session unchanged. Measured and now tested: test_a_failure_before_the_loop_saves_nothing_and_keeps_the_session_usable (the thought log cannot be opened; with --session-id: exit 1, no stdout although --output json is set, no provider request, no session row, and the same id starts normally afterward; with --resume: the row is byte-equal before and after, and a later resume works). The allowlist-refusal early return itself needs a worker, and sessions conflict with --worker-config, so it is not reachable with a session. Sessions with run-policy options are now tested: test_a_session_keeps_its_tool_allowlist (the offered tools are the two listed tools, a resume with the same allowlist sends an equal tool list, a resume without it is refused with session_tools_changed and sends nothing) and test_the_read_only_policy_belongs_to_the_run_not_to_the_session. That last test records a behavior to know: --workspace-read-only does not change the offered tools or the system prompt (it refuses a write when the tool runs), so the prefix is equal and a resume may use another setting than the first run. The session record holds the request prefix, not the run policy. docs/runtime-modes.md states this.

### Revised Code / Diff
diff --git a/docs/runtime-modes.md b/docs/runtime-modes.md
index d0c9c70..efcb5ae 100644
--- a/docs/runtime-modes.md
+++ b/docs/runtime-modes.md
@@ -59,12 +59,26 @@ Combinations with the reviewer-runtime options:
 - `--session-id` and `--resume` are refused with `--final-output`,
   `--model-profile`, and `--worker-config`. These combinations are not built.
   The session record does not hold the endpoint of a model profile.
-- `--output json` is refused with `--final-output`. Both options own stdout.
-  ADR-036 Decision 14 defines the combination. It is not built.
+- `--output json` is refused with `--final-output` and with
+  `--capability-probe-payload` (a probe requires `--final-output`). Both
+  `--output json` and `--final-output` own stdout. ADR-036 Decision 14 defines
+  the combination. It is not built.
 - `--output json` works with `--model-profile`. The field `provider` is then
   the `provider_label` of the profile.
-- `--output json` with `--worker-config` or `--audit-path` is accepted. No test
-  covers these two combinations. The top-level `--session-id` (before
+- `--output json` works with `--audit-path`. The run writes the `run_end`
+  audit event before the document reaches stdout. A failed session save still
+  prints the document, exits with code 1, and is recorded as `error`.
+- `--output json` works with `--worker-config`. A worker that is lost during
+  the run gives a document with `stop_reason: "error"` and exit code 22. A
+  worker run that does not start (a worker cannot start, or an allowlist entry
+  names no worker tool) prints no document and exits with code 22 or 2.
+- A session works with `--tool-allowlist`. The allowlist changes the offered
+  tools, so a resume needs the same allowlist.
+- `--workspace-read-only` does not change the offered tools. It belongs to the
+  run, not to the session: a resume can use another setting than the first run.
+- A run that fails before the agent loop (for example, the thought log cannot
+  be opened) saves nothing. The id of a new session stays free, and a resumed
+  session is unchanged. The top-level `--session-id` (before
 `agent`) is a different option: it filters `--stats`.
 
 **Sessions**
diff --git a/src/agent_cmd.rs b/src/agent_cmd.rs
index 7a1c5c1..21aa969 100644
--- a/src/agent_cmd.rs
+++ b/src/agent_cmd.rs
@@ -502,6 +502,15 @@ pub async fn run_agent_with(
     // ADR-036 Decision 5: a resumed session keeps its request prefix. It runs
     // on the saved model and thinking level with the saved system prompt, and
     // a different tool list is refused before any provider call.
+    //
+    // Order: `tools` is complete here. It already holds a final-answer tool
+    // (WP05), so a session with `--final-output` would digest that tool too.
+    // The CLI refuses that combination today. If ADR-036 Decision 14 is built,
+    // decide if the final-answer tool is a part of the saved prefix.
+    debug_assert!(
+        options.session.is_none() || args.final_output.is_none(),
+        "a one-shot session with --final-output is refused by the CLI"
+    );
     if let Some(session) = &options.session {
         session.check_tools(&tools)?;
     }
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
index 3a637af..fc4e4c3 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -361,6 +361,9 @@ pub async fn run_agent(
     let session_request = oneshot_session_id
         .map(crate::oneshot_session::SessionRequest::Start)
         .or(resume.map(crate::oneshot_session::SessionRequest::Resume));
+    // One-shot sessions exist only in the print path below. They are not sent
+    // to the session daemon and are not a part of its launch identity
+    // (`session_bootstrap`): the options conflict with `--interactive`.
     if let Some(request) = &session_request {
         // ADR-036: a bad id is refused before any store or provider access.
         crate::oneshot_session::validate_id(request.id())?;
diff --git a/src/cli.rs b/src/cli.rs
index 3433ab0..8f9ef6e 100644
--- a/src/cli.rs
+++ b/src/cli.rs
@@ -121,7 +121,14 @@ pub struct AgentArgs {
         // `final_output`: ADR-036 Decision 14 (the document carries the
         // validated final output) is not built. Until then the combination
         // is refused, because both options own stdout.
-        conflicts_with_all = ["interactive", "dry_run", "final_output"]
+        // `capability_probe_payload`: a probe requires `--final-output`, so
+        // it is refused here by name and not only through that rule.
+        conflicts_with_all = [
+            "interactive",
+            "dry_run",
+            "final_output",
+            "capability_probe_payload"
+        ]
     )]
     pub output: AgentOutputFormat,
     /// Start a named one-shot session and save it when the run ends. A later
@@ -882,6 +889,12 @@ mod tests {
             }
         }
         assert!(refused(&["--output", "json", "--final-output", "f.json"]));
+        assert!(refused(&[
+            "--output",
+            "json",
+            "--capability-probe-payload",
+            "p.json"
+        ]));
         // Combinations that stay available.
         assert!(!refused(&["--final-output", "f.json"]));
         assert!(!refused(&["--output", "json", "--model-profile", "p.json"]));
diff --git a/tests/test_oneshot_reviewer_combinations.py b/tests/test_oneshot_reviewer_combinations.py
new file mode 100644
index 0000000..4e09f8b
--- /dev/null
+++ b/tests/test_oneshot_reviewer_combinations.py
@@ -0,0 +1,213 @@
+"""ADR-036 one-shot options together with the reviewer-runtime options.
+
+The combined tree has both the one-shot sessions and `--output json` (ADR-036)
+and the reviewer-runtime options (run audit, workers, tool allowlist, read-only
+workspace). Neither side was built with the other. These tests fix the behavior
+of each combination that the CLI accepts. Refused combinations are in
+`test_agent_oneshot_sessions.py`.
+"""
+import json
+import shutil
+import subprocess
+
+from test_agent_oneshot_sessions import (
+    Harness, fake_provider, held, http_error, only_json_document, refusal, session_rows, text_reply,
+    wait_for_requests,
+)
+from test_worker_isolation import CRASH_CALL, DONE, NOTE_CALL, _run as run_with_worker, _write_worker
+
+__all__ = ["fake_provider"]  # the fixture of the imported module
+
+
+def state_harness(daimonos_binary, tmp_path, port):
+    """A harness with a selected state root. The audit path must be inside it."""
+    harness = Harness(daimonos_binary, tmp_path, port)
+    harness.config.write_text("[analytics]\nenabled=false\n")
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+    audit = state / "audit" / "run.jsonl"
+    return harness, state, audit, ("--state-dir", str(state), "--audit-path", str(audit))
+
+
+def run_end(audit):
+    events = [json.loads(line) for line in audit.read_text().splitlines() if line.strip()]
+    assert events[0]["kind"] == "run_start", events[0]
+    ends = [event for event in events if event["kind"] == "run_end"]
+    assert len(ends) == 1 and events[-1] is ends[0], events
+    return ends[0]["status"], ends[0]["exit_code"]
+
+
+# --- run audit (WP08): stdout is buffered until the run_end event is written ---
+
+def test_audit_with_output_json_prints_one_document_and_a_matching_run_end(daimonos_binary, tmp_path, fake_provider):
+    for index, (reply, stop_reason, exit_code, status) in enumerate([
+            (text_reply("FINE"), "end_turn", 0, "ok"),
+            (http_error(400, "bad request from the fake provider"), "error", 1, "error")]):
+        provider = fake_provider([reply])
+        harness, _, audit, global_args = state_harness(daimonos_binary, tmp_path / str(index), provider.port)
+        done = harness.agent("--output", "json", "task", global_args=global_args)
+        assert done.returncode == exit_code, done.stderr
+        assert only_json_document(done.stdout)["stop_reason"] == stop_reason
+        assert run_end(audit) == (status, exit_code)
+
+
+def test_audit_with_a_session_saves_and_resumes(daimonos_binary, tmp_path, fake_provider):
+    provider = fake_provider([text_reply("FIRST"), text_reply("SECOND")])
+    harness, state, audit, global_args = state_harness(daimonos_binary, tmp_path, provider.port)
+    first = harness.agent("--session-id", "run-1", "--output", "json", "first task", global_args=global_args)
+    assert first.returncode == 0, first.stderr
+    assert only_json_document(first.stdout)["session_saved"] is True
+    assert run_end(audit) == ("ok", 0)
+    audit.unlink()
+    second = harness.agent("--resume", "run-1", "more", global_args=global_args)
+    assert second.returncode == 0, second.stderr
+    assert second.stdout == "SECOND\n"
+    assert run_end(audit) == ("ok", 0)
+    rows = session_rows(state / "sessions" / "oneshot" / "sessions.sqlite3")
+    assert [(row[0], row[1]) for row in rows] == [("run-1", 2)]
+
+
+def test_audit_with_a_failed_session_save_still_prints_the_document(daimonos_binary, tmp_path, fake_provider):
+    # The run function returns an error after it wrote the document into the
+    # audit buffer. The buffer must reach stdout, the exit code is 1, and the
+    # run_end event records the same failure.
+    slow, gate = held(text_reply("FIRST"))
+    provider = fake_provider([slow])
+    harness, state, audit, global_args = state_harness(daimonos_binary, tmp_path, provider.port)
+    running = subprocess.Popen(
+        harness.command("--session-id", "run-1", "--output", "json", "task", global_args=global_args),
+        cwd=harness.workspace, env=harness.environment(), stdin=subprocess.DEVNULL,
+        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
+    try:
+        wait_for_requests(provider, 1)
+        shutil.rmtree(state / "sessions" / "oneshot")
+    finally:
+        gate.set()
+        stdout, stderr = running.communicate(timeout=30)
+    assert running.returncode == 1, stderr
+    doc = only_json_document(stdout)
+    assert (doc["stop_reason"], doc["session_id"], doc["session_saved"]) == ("end_turn", "run-1", False)
+    assert "session run-1 was not saved" in stderr
+    assert run_end(audit) == ("error", 1)
+
+
+# --- workers (WP10) ---
+
+def test_output_json_with_a_lost_worker_prints_the_document_and_exits_22(daimonos_binary, tmp_path):
+    for index, with_audit in enumerate([False, True]):
+        case = tmp_path / str(index)
+        case.mkdir()
+        config, _, calls = _write_worker(case)
+        audit = case / "state" / "audit.jsonl" if with_audit else None
+        done = run_with_worker(
+            daimonos_binary, case, [{"calls": [CRASH_CALL]}, {"calls": [NOTE_CALL]}, DONE],
+            worker_config=config, audit_path=audit, extra_agent_args=("--output", "json"))
+        # The loop returned a result (the worker died during the run), so the
+        # document is printed. The exit code is the worker code of WP10.
+        assert done.returncode == 22, done.stderr
+        doc = only_json_document(done.stdout)
+        assert (doc["stop_reason"], doc["final_text"]) == ("error", "")
+        assert "worker_unavailable" in doc["error_message"]
+        errors = [json.loads(line) for line in done.stderr.splitlines() if line.startswith('{"error"')]
+        assert [error["error"]["code"] for error in errors] == ["worker_unavailable"]
+        assert "read_note" not in (calls.read_text().split() if calls.exists() else [])
+        if with_audit:
+            assert run_end(audit) == ("worker_unavailable", 22)
+
+
+def test_output_json_with_a_worker_prints_the_document_of_a_successful_run(daimonos_binary, tmp_path):
+    config, _, calls = _write_worker(tmp_path)
+    done = run_with_worker(
+        daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
+        worker_config=config, extra_agent_args=("--output", "json"))
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["stop_reason"], doc["final_text"]) == ("end_turn", "done")
+    assert "read_note" in calls.read_text().split()
+
+
+def test_output_json_prints_no_document_when_a_worker_run_does_not_start(daimonos_binary, tmp_path):
+    # A worker that cannot start, and an allowlist entry that no worker serves,
+    # end the run before the agent loop. The loop returned no result, so no
+    # document is printed. The exit codes are those of WP10.
+    config = tmp_path / "gone.json"
+    config.write_text(json.dumps({"workers": {"gone": {"command": str(tmp_path / "does-not-exist"), "env": {}}}}))
+    gone = tmp_path / "gone"
+    gone.mkdir()
+    done = run_with_worker(daimonos_binary, gone, [DONE], worker_config=config,
+                           extra_agent_args=("--output", "json"))
+    assert (done.returncode, done.stdout) == (22, ""), done.stderr
+
+    unknown = tmp_path / "unknown"
+    unknown.mkdir()
+    config, _, _ = _write_worker(unknown)
+    done = run_with_worker(daimonos_binary, unknown, [DONE], worker_config=config,
+                           allowlist=["mcp__toolside__read_note", "mcp__ghost__missing"],
+                           extra_agent_args=("--output", "json"))
+    assert (done.returncode, done.stdout) == (2, ""), done.stderr
+    assert "tool_allowlist_unknown_entry" in done.stderr
+
+
+# --- sessions with run policy options ---
+
+def test_a_session_keeps_its_tool_allowlist(daimonos_binary, tmp_path, fake_provider):
+    provider = fake_provider([text_reply("A"), text_reply("B"), text_reply("NEVER SENT")])
+    harness = Harness(daimonos_binary, tmp_path, provider.port)
+    allow = tmp_path / "allow.json"
+    allow.write_text(json.dumps(["read_file", "search"]))
+    with_list = ("--tool-allowlist", str(allow))
+
+    assert harness.agent("--session-id", "run-1", "task", global_args=with_list).returncode == 0
+    offered = [tool["function"]["name"] for tool in provider.requests[0]["tools"]]
+    assert offered == ["read_file", "search"]
+    assert harness.agent("--resume", "run-1", "more", global_args=with_list).returncode == 0
+    assert json.dumps(provider.requests[1]["tools"]) == json.dumps(provider.requests[0]["tools"])
+    # Without the allowlist the tool list differs, so the prefix cannot be reproduced.
+    assert refusal(harness.agent("--resume", "run-1", "more"))["reason"] == "session_tools_changed"
+    assert len(provider.requests) == 2
+
+
+def test_the_read_only_policy_belongs_to_the_run_not_to_the_session(daimonos_binary, tmp_path, fake_provider):
+    # `--workspace-read-only` does not change the offered tools: it refuses a
+    # write when the tool runs. So the request prefix is equal with and without
+    # it, and a resume can use another policy than the first run. The session
+    # record holds the request prefix, not the run policy.
+    provider = fake_provider([text_reply("A"), text_reply("B")])
+    harness = Harness(daimonos_binary, tmp_path, provider.port)
+    assert harness.agent("--session-id", "run-1", "task", global_args=("--workspace-read-only",)).returncode == 0
+    done = harness.agent("--resume", "run-1", "more")
+    assert done.returncode == 0, done.stderr
+    assert json.dumps(provider.requests[1]["tools"]) == json.dumps(provider.requests[0]["tools"])
+    assert json.dumps(provider.requests[1]["messages"][0]) == json.dumps(provider.requests[0]["messages"][0])
+
+
+# --- a failure before the agent loop, after the session was claimed ---
+
+def test_a_failure_before_the_loop_saves_nothing_and_keeps_the_session_usable(daimonos_binary, tmp_path, fake_provider):
+    provider = fake_provider([text_reply("FIRST"), text_reply("SECOND")])
+    harness = Harness(daimonos_binary, tmp_path, provider.port)
+    database = harness.home / ".daimonos" / "oneshot-sessions" / "sessions.sqlite3"
+    # The thought log cannot be opened: its parent is a regular file. The run
+    # ends after the session was claimed and before the first model request.
+    blocker = tmp_path / "a-file"
+    blocker.write_text("")
+    bad_log = ("--debug-thoughts", "--debug-thoughts-path", str(blocker / "thoughts.log"))
+
+    failed = harness.agent("--session-id", "run-1", "--output", "json", *bad_log, "task")
+    assert failed.returncode == 1, failed.stderr
+    assert failed.stdout == "", "the loop returned no result, so no document is printed"
+    assert "thought log" in failed.stderr
+    assert provider.requests == []
+    assert session_rows(database) == [], "a run that did not reach the loop consumes no id"
+
+    started = harness.agent("--session-id", "run-1", "task")
+    assert started.returncode == 0, started.stderr
+    before = session_rows(database)
+    assert [(row[0], row[1]) for row in before] == [("run-1", 1)]
+
+    failed = harness.agent("--resume", "run-1", *bad_log, "more")
+    assert failed.returncode == 1, failed.stderr
+    assert session_rows(database) == before, "a failed resume before the loop changes nothing"
+    resumed = harness.agent("--resume", "run-1", "more")
+    assert resumed.returncode == 0, resumed.stderr
+    assert resumed.stdout == "SECOND\n"
diff --git a/tests/test_worker_isolation.py b/tests/test_worker_isolation.py
index 7b2ad94..5cb3be6 100644
--- a/tests/test_worker_isolation.py
+++ b/tests/test_worker_isolation.py
@@ -296,7 +296,8 @@ def _offered_tool_names(body):
 
 def _run(binary, tmp_path, turns, *, worker_config=None, allowlist=None,
          audit_path=None, read_only=False, task="do the work",
-         final_output=None, extra_config="", port_file=None, offers_out=None):
+         final_output=None, extra_config="", port_file=None, offers_out=None,
+         extra_agent_args=()):
     mock = _MockChat(turns)
     try:
         if port_file is not None:
@@ -338,7 +339,7 @@ def _run(binary, tmp_path, turns, *, worker_config=None, allowlist=None,
             argv += ["--worker-config", str(worker_config)]
         if final_output is not None:
             argv += ["--final-output", str(final_output)]
-        argv += ["--agent-env", str(agent_env), "--print", task]
+        argv += [*extra_agent_args, "--agent-env", str(agent_env), "--print", task]
         # The credential is a real env var of the MODEL-SIDE process, so clearing
         # it for the worker is a meaningful guarantee (AC-05.1), plus a second
         # independent carrier that is not a known provider var.

### New Points
R1. S1 not applied: under Unreleased the merged CHANGELOG has exactly one `### Added` (line 10) and one `### Changed` (line 266); the master sections start at `## [0.1.6]`. The two arrangements in the request text were the Unreleased block and the 0.1.6 block, which are separate releases. My description of it as a known concern was imprecise.
R2. S2 applied: a comment and a debug_assert at the check_tools call state that a session with --final-output is refused by the CLI and that Decision 14 must decide if the final-answer tool is a part of the saved prefix.
R3. S3 applied: the 'no test covers' sentence is gone; docs/runtime-modes.md lists each combination with its behavior, including --capability-probe-payload.
R4. S4 applied: a comment in agent_runtime.rs states that one-shot sessions exist only in the print path, are not sent to the session daemon, and are not a part of its launch identity.
R5. Verification of cb26c57 on the worker host: fmt and clippy exit 0. cargo test --no-fail-fast: 2304 passed, 6 failed (plugins::shellcheck::tests::*, no shellcheck on the host), 10 ignored. The six related pytest files (new combinations file, one-shot sessions, worker isolation, run audit, combined reviewer profile, capability probe): 129 passed. The complete pytest directory was run on dfa20e5 (924 passed, 7 shellcheck failures, 1 skipped), not again on cb26c57. CI on the merge commit dfa20e5 passed all 7 jobs; CI on cb26c57 was running at the time of this rebuttal. The 9 new tests passed at their first run because they fix existing behavior of the merge; one of them was checked by mutation (B1).

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re B1: Accepted disposition verified. The measured error path (finalize_run_audit receives the Err, run_terminal_status maps it to ('error', 1), the buffer reaches stdout before `result?` propagates, and exit_on_config_invalid / exit_on_worker_failure are no-ops for this Err class) matches the parents' contracts. The new process test test_audit_with_a_failed_session_save_still_prints_the_document exercises exactly the feared path — Err after the document is in the buffer — and asserts one JSON document on stdout with session_saved false, exit 1, the stderr message, and the matching run_end event. The mutation check (buffer written only on Ok fails the test) demonstrates the assertion has teeth. The two companion tests fix the ordering contract on the ok and provider-error paths. Resolved.
Re B2: Accepted disposition verified, including the premise correction: the probe's capability_probe_requires_final_output startup refusal combined with the json/final-output conflict already made `--output json --capability-probe-payload` unreachable, and option (b) now makes the refusal explicit and independent of that two-step chain (clap conflict, cli unit test, docs line). The accepted combinations are now each tested: lost-worker exit 22 with a document (with and without --audit-path, including the run_end record), successful worker run with a document, and the no-document rule for pre-loop worker failures (exit 22 / exit 2 with empty stdout), which gives the 'loop returned a result' rule a concrete, documented boundary. docs/runtime-modes.md no longer claims untested combinations. Resolved.
Re B3: Accepted disposition verified. The analysis resolves the core worry: open_oneshot_session takes the owner lock and checks for an existing record but writes nothing, so a pre-loop early return releases the lock and consumes no id — confirmed by test_a_failure_before_the_loop_saves_nothing_and_keeps_the_session_usable for both a new id (id stays free, reusable) and a resume (row byte-equal, later resume works). The allowlist-refusal return itself requiring a worker (and sessions conflicting with --worker-config) correctly closes that sub-path. Sessions with --tool-allowlist are now tested end-to-end including the session_tools_changed refusal on a mismatched resume, and the read-only test plus the docs note usefully pin down that --workspace-read-only is a run property, not part of the saved prefix. Resolved.
Re R1: Accepted. Your correction stands: with one `### Added` and one `### Changed` under Unreleased and the master sections living under `## [0.1.6]`, there is nothing to re-sort. S1 is withdrawn as based on an imprecise reading of the original known-concern text; no change needed.
Re R2: Accepted. The comment plus debug_assert at the check_tools call makes the invariant explicit and leaves a clear decision point for ADR-036 Decision 14 (whether the final-answer tool belongs to the saved prefix). S2 is satisfied.
Re R3: Accepted. The 'no test covers' sentence is gone and the combinations table in docs/runtime-modes.md now states the behavior of each accepted combination, including the explicit --capability-probe-payload refusal. S3 is satisfied.
Re R4: Accepted. The comment in agent_runtime.rs stating that one-shot sessions exist only in the print path and are not part of the daemon launch identity is exactly the guard-rail S4 asked for. S4 is satisfied.
Re R5: Accepted. Verification on cb26c57 is adequate: fmt/clippy clean, cargo test failures limited to the known host-environment shellcheck tests, the six affected pytest files green (129 passed), and one new test validated by mutation. Not re-running the full pytest directory on cb26c57 is acceptable given the change is confined to tests, docs, comments, and one clap conflict covered by the cli unit test, and the full suite passed on dfa20e5; please confirm the running CI on cb26c57 completes green before merging.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
