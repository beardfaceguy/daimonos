# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="agent-env-acp-signals-1623-9d8eaeb" -->

<!-- event id="request" artifact path="1623-agent-env-acp-signals/artifacts/round-1-review-request.diff" sha256="c33e6b75a376ff8c84ec5b604b38cdc4677de95c374acc021e5b45109948658c" -->
## Review Request — Round 1
**Task:** 1623 — Explicit agent env file ranks above implicit agent.env files (ADR-037); ACP signal handlers before startup work; isolated ACP lifecycle test (PR #288)
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Review the diff origin/master (f937035)..9d8eaeb of PR #288. Background, measured before the change: tests/test_acp_lifecycle.py::test_acp_runs_teardown_when_signaled passed in CI and failed intermittently on a host with ~/.config/daimonos/agent.env. Chain: (1) the test started `daimonos acp --agent-env <test file>` with the real HOME; (2) at startup, main.rs calls env_file::load_default, which reads <workspace>/agent.env and then the home agent.env and sets each variable that the process environment lacks; AgentEnv::load then reads the file selected by --agent-env and overlays non-empty process DAIMONOS_AGENT_* values, so a value that the home file had put into the environment replaced the value of the explicitly selected file (measured: a fake endpoint named in the explicit file got 0 of 5 requests with the real HOME and 5 of 5 with an empty HOME); (3) startup then sent the model-catalog request to the real provider of the home file; (4) acp installed its SIGTERM/SIGHUP/SIGINT handlers only after that startup work, inside acp_cmd::run_acp; (5) the test sent a signal 1.0 s after the spawn, and without handlers the process died by the signal (exit -15, -2, -1) with no teardown. Three changes, decided by the maintainer. FIX A (ADR-037, new file docs/adr/037-...): env_file records the names of the variables that it set (a process-global Mutex<BTreeSet<String>>, env_file::implicit_keys). AgentEnv::load splits the process DAIMONOS_AGENT_* / DAIMONOS_SKILL_DIR values into `real` (not in that set) and `implicit` (in the set). agent_env::explicit_selection: the selection is explicit when --agent-env is given, or when $DAIMONOS_AGENT_ENV is set and was not set by an implicit file. agent_env::layered_vars: with an explicit selection, implicit values only fill settings that the selected file omits or leaves blank (fill_unset_from_implicit), then real values override; without an explicit selection the old behavior holds (implicit and real both override the file). session_bootstrap::daemon_command removes the implicit variables from the environment of the session daemon child, so that the daemon (same binary, same --workspace/--state-dir/--confine options) loads the implicit files itself and has its own record; otherwise the daemon would see the parent's implicit values as real values. FIX B: agent_runtime::run_acp installs acp_cmd::TerminationSignals (tokio unix signals for SIGTERM, SIGHUP, SIGINT) first, then runs the startup work (the new prepare_acp: load agent env, resolve the context window, discover models, open analytics) inside a tokio::select! against signals.recv(); a signal during startup logs acp_stopped_during_startup and returns Ok(()) (exit 0). The same TerminationSignals value is passed to acp_cmd::run_acp, which uses it in its existing select; the two inline signal registrations and tokio::signal::ctrl_c() there are replaced by signals.recv(). FIX C: tests/test_acp_lifecycle.py starts acp with an empty HOME, its own workspace and cwd, and waits for the ACP initialize reply in place of time.sleep(1.0). New cases: a provider endpoint that holds the model-catalog reply; the test sends each signal while the request is held and requires exit code 0 within 4 s. New process tests in tests/test_agent_env_precedence.py (8) with two fake provider endpoints: which endpoint receives the request shows which file supplied DAIMONOS_AGENT_BASE_URL. Verification by the worker: before the fixes, 7 of the new process tests failed on master (3 signal cases with -15, -2, -1; 4 precedence cases at the wrong endpoint); the daemon test fails when the env_remove is taken out. After: fmt and clippy exit 0; the Rust suite with an empty HOME: 2149 passed, 6 failed (plugins::shellcheck::tests::*, no shellcheck on the host), 10 ignored; pytest tests: 607 passed, 7 failed (tests/test_shellcheck.py, same cause), 1 skipped; the ACP lifecycle file 10 runs idle and 10 runs under 40 busy processes on a host with a home agent.env: 0 failed of 140 cases. CI on the PR was running at the time of this request.

### Relevant Code / Diff
diff --git a/CHANGELOG.md b/CHANGELOG.md
index fbfc69c..5f2d42a 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -7,6 +7,22 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ## [Unreleased]
 
+### Fixed
+
+- **An explicit `--agent-env` file lost to the home `agent.env`** (ADR-037,
+  vikunja #1623). Daimonos loads `<workspace>/agent.env` and
+  `~/.config/daimonos/agent.env` into the process environment at startup, and
+  their values then replaced the values of the file that `--agent-env` or
+  `$DAIMONOS_AGENT_ENV` names. A run with `--agent-env` on a host with a home
+  `agent.env` used the provider endpoint, the model, and the API key of the
+  home file. The order is now: real environment, then the explicitly selected
+  file, then the implicit files. A run without an explicit selection does not
+  change.
+- **A signal during `daimonos acp` startup killed the process** (vikunja
+  #1623). The handlers for SIGTERM, SIGINT, and SIGHUP were installed after the
+  startup requests to the provider. They are now installed first, and a signal
+  during startup ends the process with exit code 0.
+
 ### Changed
 
 - **Releases start from CI.** A merge to `master` that changes the version in
diff --git a/docs/adr/037-explicit-agent-env-ranks-above-implicit-files.md b/docs/adr/037-explicit-agent-env-ranks-above-implicit-files.md
new file mode 100644
index 0000000..8702459
--- /dev/null
+++ b/docs/adr/037-explicit-agent-env-ranks-above-implicit-files.md
@@ -0,0 +1,80 @@
+# ADR-037: An explicitly selected agent env file ranks above the implicit `agent.env` files
+
+- **Status:** Accepted
+- **Date:** 2026-11-23
+- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-23 ("do all three", after the options were shown).
+- **Consulted:** None recorded.
+- **Informed:** None recorded.
+- **Spec:** Vikunja #1623.
+- **Dependencies:** ADR-021 (state root: no home `agent.env`), ADR-022 (confinement: workspace `agent.env` only).
+- **Supersedes:** None
+- **Superseded by:** None
+
+## Context
+
+Two mechanisms read `agent.env` files.
+
+- `env_file` runs first, for each runtime mode. It reads the **implicit** files `<workspace>/agent.env` and `~/.config/daimonos/agent.env` and sets each variable that the process environment does not have.
+- `agent_env` then reads the **selected** file for `agent`, `chat`, `acp`, and the session daemon. The selection is `--agent-env`, else `$DAIMONOS_AGENT_ENV`, else the home file. Non-empty process values named `DAIMONOS_AGENT_*` override the values of the selected file.
+
+The second step cannot tell a value of the launcher from a value that the first step set. So a value of an implicit file replaced the value of the file that the caller named with `--agent-env`.
+
+Measured on 2026-11-23: with a home `agent.env`, a run with `--agent-env <file>` sent its requests to the endpoint, with the model and the API key, of the home file. An unattended caller that passes `--agent-env` (an orchestrator, a test) got the settings of the developer host.
+
+How must an explicitly selected agent env file rank against the implicit files?
+
+## Decision drivers
+
+- A caller that names a file must get the settings of that file.
+- A launcher that sets a variable in the real environment must keep its override (the ACP `env` block of an editor).
+- A run without an explicit selection must not change.
+- No credential of a host file may reach an endpoint that an explicit file names, and the reverse.
+
+## Decision
+
+We will rank the settings of an agent run in this order, highest first:
+
+1. CLI behavior options, for example `--model`.
+2. The real environment: a non-empty `DAIMONOS_AGENT_*` variable that the launcher supplied.
+3. The explicitly selected file.
+4. The implicit `agent.env` files.
+
+A file is explicitly selected when `--agent-env` names it, or when `$DAIMONOS_AGENT_ENV` of the real environment names it. A selector that an implicit file supplies is not an explicit selection.
+
+- `env_file` records the name of each variable that it sets (`env_file::implicit_keys`). `agent_env` uses that record to split the process values into real values and implicit values.
+- With an explicit selection, an implicit value only fills a setting that the selected file omits or leaves blank.
+- Without an explicit selection, the behavior does not change. The selected file is then the home file, and each process value overrides it.
+- The session daemon does not inherit the implicit values of the process that starts it. It loads the implicit files itself, so it has its own record.
+- The rule covers the settings that `agent_env` parses: `DAIMONOS_AGENT_*` and `DAIMONOS_SKILL_DIR`.
+
+## Alternatives
+
+- **Do not load the implicit files when a file is explicitly selected.** This is the cleanest isolation. It loses: an implicit file also supplies variables that `agent_env` does not parse (see Consequences) and variables for tool child processes. The loss of those would change more runs.
+- **Let the explicit file win over the real environment too.** It loses: an editor that starts `acp` with an `env` block and an `--agent-env` option would lose its override, against the rule of `env_file` that the launcher wins.
+- **Keep the order and change the documents.** It loses: a named file that a host file overrides cannot give a reproducible run.
+- **Clear the environment in each caller.** It works for a test. It loses as a product rule: each caller must know the rule, and a caller with a home file that omits it sends credentials to the wrong endpoint.
+
+## Consequences
+
+- A run with `--agent-env` on a host with a home `agent.env` changes: the named file now wins for each setting that it sets. This is the intended change.
+- A setting that the named file omits still comes from an implicit file. A caller that needs a closed set of settings uses `--state-dir`, which reads no home file (ADR-021).
+- Not covered: variables that the product reads directly from the process environment and that `agent_env` does not parse. They are `DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`, `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, and `DAIMONOS_AGENT_SESSION_ID`. For these, a line in the selected file has no effect, as before, and a value of an implicit file or of the real environment applies.
+- The record of implicit variables is process state. A process that another Daimonos process starts, and that inherits its environment, sees the implicit values as real values. The session daemon is the one such child, and its start removes them.
+
+## Confirmation
+
+- `pytest tests/test_agent_env_precedence.py` drives the real binary with two provider endpoints. It covers: the explicit file against the home file, against the workspace file with `--state-dir`, the real environment above the explicit file, an omitted setting filled from the home file, `$DAIMONOS_AGENT_ENV` of the real environment, a selector from an implicit file, the unchanged run without a selection, and the session daemon of an interactive run.
+- `cargo test agent_env::` covers the order in `layered_vars`, the fill rule, and `explicit_selection`.
+- `cargo test daemon_command_does_not_pass_on_implicit_agent_env_values` covers the environment of the session daemon.
+
+## Anchors
+
+_Anchors:_ src/env_file.rs::implicit_keys, src/agent_env.rs::AgentEnv::load, src/agent_env.rs::layered_vars, src/agent_env.rs::explicit_selection, src/agent_env.rs::fill_unset_from_implicit, src/session_bootstrap.rs::daemon_command
+
+## Revisit when
+
+A second Daimonos child process inherits the environment of its parent and reads agent settings, or a caller needs an explicit file that reads no implicit file without `--state-dir`.
+
+## Open questions
+
+1. Should the five directly read variables in Consequences move into `agent_env`, so that the selected file can set them? Not decided.
diff --git a/docs/configuration.md b/docs/configuration.md
index 71c9eb1..6182480 100644
--- a/docs/configuration.md
+++ b/docs/configuration.md
@@ -121,6 +121,33 @@ CLI behavior flags (for example --model) > process DAIMONOS_AGENT_* values
 > selected agent.env file values
 ```
 
+Daimonos also loads two **implicit** files into the process environment at
+startup: `<workspace>/agent.env`, then `~/.config/daimonos/agent.env`. They set
+only variables that the environment does not have. With `--state-dir` the home
+file is not read. With confinement only the workspace file is read.
+
+When the file is selected explicitly, with `--agent-env` or with
+`$DAIMONOS_AGENT_ENV` in the real environment, a value from an implicit file
+ranks below the selected file (ADR-037):
+
+```text
+CLI behavior flags > real environment DAIMONOS_AGENT_* values
+> explicitly selected agent.env file > implicit agent.env files
+```
+
+- A value from an implicit file fills only a setting that the selected file
+  omits or leaves blank.
+- Without an explicit selection, nothing changes: the selected file is the home
+  file, and each process value overrides it.
+- A selector that an implicit file sets (`DAIMONOS_AGENT_ENV=...` in
+  `<workspace>/agent.env`) is not an explicit selection.
+- The rule covers `DAIMONOS_AGENT_*` settings that the agent env file defines,
+  and `DAIMONOS_SKILL_DIR`. Five variables are read from the process
+  environment only, so a line in the selected file has no effect on them:
+  `DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`,
+  `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, and
+  `DAIMONOS_AGENT_SESSION_ID`.
+
 Required effective values remain
 `DAIMONOS_AGENT_PROVIDER`, `DAIMONOS_AGENT_MODEL`,
 `DAIMONOS_AGENT_BASE_URL`, `DAIMONOS_AGENT_APPROVAL_MODE`,
diff --git a/src/acp_cmd.rs b/src/acp_cmd.rs
index 748690d..21bdf93 100644
--- a/src/acp_cmd.rs
+++ b/src/acp_cmd.rs
@@ -3879,6 +3879,38 @@ fn build_agent_with_state(
         )
 }
 
+/// The signals that end the ACP engine: SIGTERM, SIGHUP, and SIGINT.
+///
+/// The caller installs them before the startup work that waits for the
+/// provider (model discovery), so a signal during startup does not use the
+/// default action (vikunja #1623). A signal that arrives after `install` is
+/// kept until the next `recv`.
+pub struct TerminationSignals {
+    terminate: tokio::signal::unix::Signal,
+    hangup: tokio::signal::unix::Signal,
+    interrupt: tokio::signal::unix::Signal,
+}
+
+impl TerminationSignals {
+    pub fn install() -> std::io::Result<Self> {
+        use tokio::signal::unix::{signal, SignalKind};
+        Ok(Self {
+            terminate: signal(SignalKind::terminate())?,
+            hangup: signal(SignalKind::hangup())?,
+            interrupt: signal(SignalKind::interrupt())?,
+        })
+    }
+
+    /// Wait for one of the signals and return its name.
+    pub async fn recv(&mut self) -> &'static str {
+        tokio::select! {
+            _ = self.terminate.recv() => "SIGTERM",
+            _ = self.hangup.recv() => "SIGHUP",
+            _ = self.interrupt.recv() => "SIGINT",
+        }
+    }
+}
+
 /// Run the `daimonos acp` engine to completion (until stdin closes).
 /// `make_provider` builds a fresh provider per session (Zed keeps one
 /// process across chat threads); it's validated once up front so a
@@ -3897,6 +3929,7 @@ pub async fn run_acp(
     analytics: Option<Arc<AnalyticsStore>>,
     timestamp_turns: bool,
     thinking: crate::providers::ThinkingLevel,
+    mut signals: TerminationSignals,
 ) -> anyhow::Result<()> {
     tracing::info!(
         target: "daimonos::acp",
@@ -3973,14 +4006,10 @@ pub async fn run_acp(
     tokio::pin!(eof_wait);
     // A client that stops the engine by signal gets the same teardown as EOF;
     // the default handlers would orphan running tool process groups (#1544).
-    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
-    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
     let (result, input_error, signal) = tokio::select! {
         result = &mut connection => (Some(result), None, None),
         input_error = &mut eof_wait => (None, input_error, None),
-        _ = tokio::signal::ctrl_c() => (None, None, Some("SIGINT")),
-        _ = terminate.recv() => (None, None, Some("SIGTERM")),
-        _ = hangup.recv() => (None, None, Some("SIGHUP")),
+        signal = signals.recv() => (None, None, Some(signal)),
     };
     tracing::info!(
         target: "daimonos::acp",
diff --git a/src/agent_env.rs b/src/agent_env.rs
index 469b287..6e3f096 100644
--- a/src/agent_env.rs
+++ b/src/agent_env.rs
@@ -11,6 +11,11 @@
 //!   `--agent-env <path>`  >  `$DAIMONOS_AGENT_ENV`  >  `~/.config/daimonos/agent.env`
 //! Value precedence after selecting that file:
 //!   non-empty process `$DAIMONOS_AGENT_*`  >  value parsed from the file
+//!
+//! With an explicitly selected file (`--agent-env` or `$DAIMONOS_AGENT_ENV`),
+//! a process value that an implicit `agent.env` file supplied ranks below the
+//! selected file (ADR-037, `env_file::implicit_keys`):
+//!   real environment  >  selected file  >  implicit `agent.env` files
 
 use std::collections::HashMap;
 use std::path::{Path, PathBuf};
@@ -142,16 +147,28 @@ impl AgentEnv {
     pub fn load(flag: Option<PathBuf>) -> Result<AgentEnv, String> {
         // Use vars_os so an unrelated non-Unicode process variable cannot
         // panic agent startup. Non-Unicode agent keys/values are ignored.
-        let process_vars: HashMap<String, String> = std::env::vars_os()
-            .filter_map(|(key, value)| {
-                let key = key.into_string().ok()?;
-                if !key.starts_with("DAIMONOS_AGENT_") && key != crate::skills::SKILL_DIR_ENV {
-                    return None;
-                }
-                Some((key, value.into_string().ok()?))
-            })
-            .collect();
-        Self::load_with_process_vars(flag, &process_vars)
+        let implicit_keys = crate::env_file::implicit_keys();
+        let mut real = HashMap::new();
+        let mut implicit = HashMap::new();
+        for (key, value) in std::env::vars_os() {
+            let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) else {
+                continue;
+            };
+            if !key.starts_with("DAIMONOS_AGENT_") && key != crate::skills::SKILL_DIR_ENV {
+                continue;
+            }
+            if implicit_keys.contains(&key) {
+                implicit.insert(key, value);
+            } else {
+                real.insert(key, value);
+            }
+        }
+        let explicit = explicit_selection(
+            flag.is_some(),
+            std::env::var_os("DAIMONOS_AGENT_ENV").is_some(),
+            implicit_keys.contains("DAIMONOS_AGENT_ENV"),
+        );
+        Self::load_layered(flag, explicit, &real, &implicit)
     }
 
     /// Load only an explicitly selected agent env file under ADR-021.
@@ -164,9 +181,17 @@ impl AgentEnv {
 
     /// Injectable load seam used by tests to verify real file + process
     /// precedence without mutating the process-global environment.
-    fn load_with_process_vars(
+    ///
+    /// `real` holds the process values that the launcher supplied. `implicit`
+    /// holds the process values that an implicit `agent.env` file supplied.
+    /// ADR-037: with an explicitly selected file (`explicit`), an implicit
+    /// value only fills a setting that the selected file does not set.
+    /// Without one, both kinds override the selected file as before.
+    fn load_layered(
         flag: Option<PathBuf>,
-        process_vars: &HashMap<String, String>,
+        explicit: bool,
+        real: &HashMap<String, String>,
+        implicit: &HashMap<String, String>,
     ) -> Result<AgentEnv, String> {
         let path = Self::resolve_path(flag).ok_or_else(|| {
             "cannot resolve agent env file path — set --agent-env or $DAIMONOS_AGENT_ENV, \
@@ -181,8 +206,7 @@ impl AgentEnv {
                 REQUIRED.join(", ")
             )
         })?;
-        let mut vars = parse_dotenv(&content);
-        merge_process_overrides(&mut vars, process_vars);
+        let vars = layered_vars(&content, explicit, real, implicit);
         Self::from_vars(&vars, &path)
     }
 
@@ -567,6 +591,52 @@ fn merge_process_overrides(
     }
 }
 
+/// ADR-037: the caller selected the agent env file explicitly. That is so with
+/// `--agent-env`, and with `$DAIMONOS_AGENT_ENV` from the real environment. A
+/// selector that an implicit `agent.env` file supplied is not an explicit
+/// selection: that file then keeps its rank above the file that it selects.
+fn explicit_selection(flag_given: bool, selector_set: bool, selector_is_implicit: bool) -> bool {
+    flag_given || (selector_set && !selector_is_implicit)
+}
+
+/// The effective settings: the selected file, then the two kinds of process
+/// values in the order of ADR-037. Pure, so tests need no process environment.
+fn layered_vars(
+    content: &str,
+    explicit: bool,
+    real: &HashMap<String, String>,
+    implicit: &HashMap<String, String>,
+) -> HashMap<String, String> {
+    let mut vars = parse_dotenv(content);
+    if explicit {
+        fill_unset_from_implicit(&mut vars, implicit);
+    } else {
+        merge_process_overrides(&mut vars, implicit);
+    }
+    merge_process_overrides(&mut vars, real);
+    vars
+}
+
+/// Use a value from an implicit `agent.env` file only for a setting that the
+/// explicitly selected file does not set (ADR-037). A blank value in the
+/// selected file does not set the setting, as in [`merge_process_overrides`].
+fn fill_unset_from_implicit(
+    file_vars: &mut HashMap<String, String>,
+    implicit: &HashMap<String, String>,
+) {
+    for (key, value) in implicit {
+        let is_agent_setting = (key.starts_with("DAIMONOS_AGENT_")
+            || key == crate::skills::SKILL_DIR_ENV)
+            && key != "DAIMONOS_AGENT_ENV";
+        let unset_in_file = file_vars
+            .get(key)
+            .is_none_or(|existing| existing.trim().is_empty());
+        if is_agent_setting && unset_in_file && !value.trim().is_empty() {
+            file_vars.insert(key.clone(), value.clone());
+        }
+    }
+}
+
 /// Build the picker's candidate model list: `current` is always present
 /// (prepended if the configured list omits it), duplicates removed while
 /// preserving first-seen order. Result is never empty.
@@ -793,11 +863,84 @@ mod tests {
             "anthropic/claude-opus-4.8".to_string(),
         )]);
 
-        let env = AgentEnv::load_with_process_vars(Some(path), &process_vars).unwrap();
+        let env = AgentEnv::load_layered(Some(path), true, &process_vars, &HashMap::new()).unwrap();
 
         assert_eq!(env.model, "anthropic/claude-opus-4.8");
     }
 
+    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
+        pairs
+            .iter()
+            .map(|(key, value)| (key.to_string(), value.to_string()))
+            .collect()
+    }
+
+    /// ADR-037: real environment > explicitly selected file > implicit files.
+    #[test]
+    fn an_explicit_file_ranks_between_the_real_environment_and_implicit_values() {
+        let dir = tempfile::tempdir().unwrap();
+        let path = dir.path().join("agent.env");
+        // The file sets the model, leaves the base URL blank, and omits the
+        // thinking level.
+        std::fs::write(
+            &path,
+            base().replace(
+                "DAIMONOS_AGENT_BASE_URL=https://openrouter.ai/api/v1\n",
+                "DAIMONOS_AGENT_BASE_URL=\n",
+            ),
+        )
+        .unwrap();
+        let implicit = vars(&[
+            ("DAIMONOS_AGENT_MODEL", "implicit/model"),
+            ("DAIMONOS_AGENT_BASE_URL", "https://implicit.example/v1"),
+            ("DAIMONOS_AGENT_THINKING", "high"),
+            ("DAIMONOS_AGENT_PROVIDER", "  "),
+            ("DAIMONOS_AGENT_ENV", "/implicit/selector"),
+        ]);
+
+        // The explicit file wins for a setting that it sets.
+        let env =
+            AgentEnv::load_layered(Some(path.clone()), true, &HashMap::new(), &implicit).unwrap();
+        assert_eq!(env.model, "anthropic/claude-sonnet-4.6");
+        assert_eq!(env.provider, "openrouter");
+        // An implicit value fills a setting that the file omits or leaves blank.
+        assert_eq!(env.base_url, "https://implicit.example/v1");
+        assert_eq!(env.thinking, ThinkingLevel::High);
+
+        // The real environment wins over the explicit file.
+        let real = vars(&[("DAIMONOS_AGENT_MODEL", "real/model")]);
+        let env = AgentEnv::load_layered(Some(path), true, &real, &implicit).unwrap();
+        assert_eq!(env.model, "real/model");
+        assert_eq!(env.base_url, "https://implicit.example/v1");
+    }
+
+    /// Without an explicit selection, the selected file is the home file, and
+    /// an implicit value overrides it as before ADR-037.
+    #[test]
+    fn without_an_explicit_selection_implicit_values_override_the_file() {
+        let implicit = vars(&[("DAIMONOS_AGENT_MODEL", "implicit/model")]);
+        let real = vars(&[("DAIMONOS_AGENT_API_KEY", "sk-real")]);
+        let model = |explicit: bool| {
+            let layered = layered_vars(&base(), explicit, &real, &implicit);
+            assert_eq!(layered["DAIMONOS_AGENT_API_KEY"], "sk-real");
+            layered["DAIMONOS_AGENT_MODEL"].clone()
+        };
+        assert_eq!(model(false), "implicit/model");
+        assert_eq!(model(true), "anthropic/claude-sonnet-4.6");
+    }
+
+    /// `--agent-env` and `$DAIMONOS_AGENT_ENV` of the real environment are
+    /// explicit selections. The same selector from an implicit file is not.
+    #[test]
+    fn only_the_option_and_the_real_selector_are_explicit_selections() {
+        // (flag given, selector set, selector is implicit) -> explicit
+        assert!(explicit_selection(true, false, false));
+        assert!(explicit_selection(true, true, true));
+        assert!(explicit_selection(false, true, false));
+        assert!(!explicit_selection(false, true, true));
+        assert!(!explicit_selection(false, false, false));
+    }
+
     #[test]
     fn empty_process_value_does_not_override_file() {
         let env = load_str_with_overrides(
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
index dad0eac..38aaf3e 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -640,6 +640,57 @@ pub async fn run_acp(
     cfg: Arc<config::Config>,
     token_log: Option<PathBuf>,
 ) -> anyhow::Result<()> {
+    // Install the handlers before the startup work that waits for the
+    // provider. A signal during startup then ends the process with code 0.
+    // No session and no tool process exists at that time, so no teardown is
+    // necessary (vikunja #1623).
+    let mut signals = acp_cmd::TerminationSignals::install()?;
+    let startup = tokio::select! {
+        startup = prepare_acp(args, cfg) => startup?,
+        signal = signals.recv() => {
+            tracing::info!(
+                target: "daimonos::acp",
+                event = "acp_stopped_during_startup",
+                signal,
+            );
+            return Ok(());
+        }
+    };
+    acp_cmd::run_acp(
+        startup.make_provider,
+        workspace,
+        startup.cfg,
+        startup.model,
+        startup.models,
+        startup.safety,
+        token_log,
+        startup.sessions_dir,
+        startup.compaction,
+        startup.analytics,
+        startup.timestamp_turns,
+        startup.thinking,
+        signals,
+    )
+    .await
+}
+
+/// What `acp` prepares before it serves its client.
+struct AcpStartup {
+    make_provider: acp_cmd::ProviderFactory,
+    cfg: Arc<config::Config>,
+    model: String,
+    models: Vec<String>,
+    safety: safety::SafetyPolicy,
+    sessions_dir: Option<PathBuf>,
+    compaction: crate::session_core::SessionCompaction,
+    analytics: Option<Arc<analytics::AnalyticsStore>>,
+    timestamp_turns: bool,
+    thinking: providers::ThinkingLevel,
+}
+
+/// The startup work of `acp`. It waits for the provider: the context window
+/// and the model catalog.
+async fn prepare_acp(args: AcpArgs, cfg: Arc<config::Config>) -> anyhow::Result<AcpStartup> {
     let AcpArgs {
         model,
         provider,
@@ -709,21 +760,21 @@ pub async fn run_acp(
     } else {
         None
     };
-    acp_cmd::run_acp(
+    Ok(AcpStartup {
         make_provider,
-        workspace,
         cfg,
-        effective_model,
+        model: effective_model,
         models,
         safety,
-        token_log,
         sessions_dir,
-        crate::session_core::SessionCompaction::new(compaction, compaction_follows_model),
-        analytics_store,
-        agent.timestamp_turns,
-        agent.thinking.clone(),
-    )
-    .await
+        compaction: crate::session_core::SessionCompaction::new(
+            compaction,
+            compaction_follows_model,
+        ),
+        analytics: analytics_store,
+        timestamp_turns: agent.timestamp_turns,
+        thinking: agent.thinking.clone(),
+    })
 }
 
 pub async fn run_session_daemon(
diff --git a/src/env_file.rs b/src/env_file.rs
index 23d5a84..2f33a0f 100644
--- a/src/env_file.rs
+++ b/src/env_file.rs
@@ -11,6 +11,11 @@
 //! A variable already present in the process environment is never overwritten,
 //! so values injected by the launcher (e.g. Zed's ACP `env` block) always win.
 //!
+//! The files of this module are the **implicit** files: no option selects
+//! them. [`implicit_keys`] names the variables that this process set from
+//! them. The agent frontends use it to rank an explicitly selected agent env
+//! file above these values (ADR-037, `agent_env`).
+//!
 //! ## Security
 //! `agent.env` typically lives in a checked-out repository — untrusted input —
 //! and daimonos' environment is inherited by every tool it exec's. To keep a
@@ -18,7 +23,22 @@
 //! that steer how executables/interpreters load are refused from the file (see
 //! [`DENYLIST`]); set those in the real environment if you truly need them.
 
+use std::collections::BTreeSet;
 use std::path::Path;
+use std::sync::Mutex;
+
+/// The variables that this process set from an implicit `agent.env` file.
+static IMPLICIT_KEYS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
+
+/// Names of the variables that this process set from an implicit `agent.env`
+/// file. A variable that the launcher supplied is never in this set: an
+/// implicit file does not overwrite it.
+pub fn implicit_keys() -> BTreeSet<String> {
+    IMPLICIT_KEYS
+        .lock()
+        .unwrap_or_else(|poisoned| poisoned.into_inner())
+        .clone()
+}
 
 /// Variables that alter loader/interpreter behaviour and are inherited by
 /// exec'd tools — classic injection vectors. Never settable from a file.
@@ -164,6 +184,10 @@ fn apply_content(content: &str, path: &Path, quiet: bool) {
         // the environment yet; mirrors `session::enhance_process_path()`.
         std::env::set_var(key, value);
     }
+    IMPLICIT_KEYS
+        .lock()
+        .unwrap_or_else(|poisoned| poisoned.into_inner())
+        .extend(outcome.set.iter().map(|(key, _)| key.clone()));
     if !quiet && !outcome.set.is_empty() {
         eprintln!(
             "daimonos: loaded {} variable(s) from {}",
diff --git a/src/session_bootstrap.rs b/src/session_bootstrap.rs
index 7040345..86b9cde 100644
--- a/src/session_bootstrap.rs
+++ b/src/session_bootstrap.rs
@@ -72,14 +72,7 @@ pub async fn connect_or_spawn(
 
     let executable = std::env::current_exe()
         .map_err(|error| anyhow::anyhow!("resolve daimonos binary: {error}"))?;
-    let mut command = tokio::process::Command::new(&executable);
-    command
-        .args(daemon_args(options))
-        .env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options))
-        .stdin(Stdio::null())
-        .stdout(Stdio::null())
-        .stderr(Stdio::null())
-        .kill_on_drop(false);
+    let mut command = daemon_command(&executable, options, &crate::env_file::implicit_keys());
     // SAFETY: this closure runs after Command's fork and before exec. setsid,
     // fork, and _exit are async-signal-safe; do not add allocation, logging, or
     // any other non-async-signal-safe operation here. The grandchild returns to
@@ -164,6 +157,32 @@ fn should_bootstrap(error: &std::io::Error) -> bool {
     )
 }
 
+/// The command that starts the session daemon, without its `pre_exec` step.
+///
+/// The daemon does not inherit the variables that this process set from an
+/// implicit `agent.env` file. It has the same workspace and state options, so
+/// it loads the same files again and knows that these values are implicit.
+/// Inherited, they would count as values of the real environment and rank
+/// above the `--agent-env` file (ADR-037).
+fn daemon_command(
+    executable: &Path,
+    options: &BootstrapOptions<'_>,
+    implicit_keys: &std::collections::BTreeSet<String>,
+) -> tokio::process::Command {
+    let mut command = tokio::process::Command::new(executable);
+    command
+        .args(daemon_args(options))
+        .stdin(Stdio::null())
+        .stdout(Stdio::null())
+        .stderr(Stdio::null())
+        .kill_on_drop(false);
+    for key in implicit_keys {
+        command.env_remove(key);
+    }
+    command.env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options));
+    command
+}
+
 fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
     let mut args = vec![
         OsString::from("--workspace"),
@@ -323,6 +342,36 @@ fn instance_metadata(socket_path: &Path) -> std::io::Result<Option<serde_json::V
 mod tests {
     use super::*;
 
+    /// ADR-037: the daemon loads the implicit `agent.env` files itself. It
+    /// does not inherit their values as values of the real environment.
+    #[test]
+    fn daemon_command_does_not_pass_on_implicit_agent_env_values() {
+        let directory = tempfile::tempdir().unwrap();
+        let config = directory.path().join("config.toml");
+        let socket = directory.path().join("daemon.sock");
+        let agent_env = directory.path().join("agent.env");
+        let selected = options(directory.path(), &config, &socket, &agent_env);
+        let implicit = ["DAIMONOS_AGENT_MODEL", "DAIMONOS_AGENT_BASE_URL"]
+            .into_iter()
+            .map(String::from)
+            .collect();
+
+        let command = daemon_command(Path::new("/bin/daimonos"), &selected, &implicit);
+        let envs: std::collections::HashMap<_, _> = command
+            .as_std()
+            .get_envs()
+            .map(|(key, value)| (key.to_owned(), value.map(|value| value.to_owned())))
+            .collect();
+        // `None` is a removal from the inherited environment.
+        assert_eq!(envs[std::ffi::OsStr::new("DAIMONOS_AGENT_MODEL")], None);
+        assert_eq!(envs[std::ffi::OsStr::new("DAIMONOS_AGENT_BASE_URL")], None);
+        assert!(envs[std::ffi::OsStr::new(BOOTSTRAP_FINGERPRINT_ENV)].is_some());
+        assert_eq!(envs.len(), 3);
+
+        let none = daemon_command(Path::new("/bin/daimonos"), &selected, &Default::default());
+        assert_eq!(none.as_std().get_envs().count(), 1);
+    }
+
     fn options<'a>(
         workspace: &'a Path,
         config: &'a Path,
diff --git a/tests/test_acp_lifecycle.py b/tests/test_acp_lifecycle.py
index 1637908..cdabde4 100644
--- a/tests/test_acp_lifecycle.py
+++ b/tests/test_acp_lifecycle.py
@@ -1,37 +1,62 @@
+"""Process lifecycle of `daimonos acp`: stdin EOF and termination signals.
+
+Each process runs with an empty HOME and its own workspace. A developer host
+can have a `~/.config/daimonos/agent.env`, and a test must not read it or send
+requests to the provider that it names (vikunja #1623).
+"""
+import json
 import signal
 import subprocess
+import threading
 import time
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
 
 import pytest
 
+SIGNALS = [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]
 
-def _write_agent_env(tmp_path):
+
+def _spawn_acp(daimonos_binary, tmp_path, base_url="http://127.0.0.1:1"):
+    workspace = tmp_path / "workspace"
+    workspace.mkdir()
+    home = tmp_path / "home"
+    home.mkdir()
     agent_env = tmp_path / "agent.env"
-    agent_env.write_text(
-        "\n".join(
-            [
-                "DAIMONOS_AGENT_PROVIDER=openrouter",
-                "DAIMONOS_AGENT_MODEL=test/model",
-                "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
-                "DAIMONOS_AGENT_APPROVAL_MODE=auto",
-                "DAIMONOS_AGENT_API_KEY=test",
-                "DAIMONOS_AGENT_COMPACTION=off",
-                "",
-            ]
-        )
-    )
-    return agent_env
-
-
-def _spawn_acp(daimonos_binary, tmp_path):
+    agent_env.write_text("\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter",
+        "DAIMONOS_AGENT_MODEL=test/model",
+        f"DAIMONOS_AGENT_BASE_URL={base_url}",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
+        "DAIMONOS_AGENT_API_KEY=test",
+        "DAIMONOS_AGENT_COMPACTION=off",
+        "",
+    ]))
     return subprocess.Popen(
-        [daimonos_binary, "acp", "--agent-env", str(_write_agent_env(tmp_path))],
+        [daimonos_binary, "--workspace", str(workspace), "acp", "--agent-env", str(agent_env)],
+        cwd=workspace,
+        env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
         stdin=subprocess.PIPE,
         stdout=subprocess.PIPE,
         stderr=subprocess.PIPE,
     )
 
 
+def _stop(process):
+    if process.poll() is None:
+        process.kill()
+        process.wait()
+
+
+def _initialize(process):
+    """Complete the ACP handshake. A reply shows that the transport loop runs."""
+    request = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
+               "params": {"protocolVersion": 1, "clientCapabilities": {}}}
+    process.stdin.write((json.dumps(request) + "\n").encode())
+    process.stdin.flush()
+    reply = json.loads(process.stdout.readline())
+    assert reply["id"] == 1 and "result" in reply, reply
+
+
 def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
     process = _spawn_acp(daimonos_binary, tmp_path)
 
@@ -40,12 +65,10 @@ def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
     try:
         assert process.wait(timeout=5) == 0
     finally:
-        if process.poll() is None:
-            process.kill()
-            process.wait()
+        _stop(process)
 
 
-@pytest.mark.parametrize("sig", [signal.SIGTERM, signal.SIGINT, signal.SIGHUP])
+@pytest.mark.parametrize("sig", SIGNALS)
 def test_acp_runs_teardown_when_signaled(daimonos_binary, tmp_path, sig):
     """Signals take the EOF teardown path instead of the default handler (#1544).
 
@@ -54,11 +77,68 @@ def test_acp_runs_teardown_when_signaled(daimonos_binary, tmp_path, sig):
     """
     process = _spawn_acp(daimonos_binary, tmp_path)
     try:
-        time.sleep(1.0)
-        assert process.poll() is None, process.stderr.read().decode()
+        _initialize(process)
         process.send_signal(sig)
         assert process.wait(timeout=10) == 0
     finally:
-        if process.poll() is None:
-            process.kill()
-            process.wait()
+        _stop(process)
+
+
+class HeldCatalog:
+    """A provider endpoint that holds its model-catalog reply until released."""
+
+    def __init__(self):
+        owner = self
+        self.arrived = threading.Event()
+        self.release = threading.Event()
+
+        class Handler(BaseHTTPRequestHandler):
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                owner.arrived.set()
+                owner.release.wait(timeout=30)
+                body = json.dumps({"data": [{"id": "test/model", "context_length": 100000}]}).encode()
+                try:
+                    self.send_response(200)
+                    self.send_header("Content-Type", "application/json")
+                    self.send_header("Content-Length", str(len(body)))
+                    self.end_headers()
+                    self.wfile.write(body)
+                except OSError:
+                    pass  # the client is gone
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        threading.Thread(target=self.server.serve_forever, daemon=True).start()
+        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
+
+    def close(self):
+        self.release.set()
+        self.server.shutdown()
+        self.server.server_close()
+
+
+@pytest.mark.parametrize("sig", SIGNALS)
+def test_acp_exits_cleanly_when_signaled_during_startup(daimonos_binary, tmp_path, sig):
+    """A signal that arrives while startup waits for the provider ends the
+    process with code 0, and at once (#1623).
+
+    Startup asks the provider for its model catalog. Before the fix, the signal
+    handlers were installed after that request, so a signal during the request
+    used the default action: exit by signal, with no teardown.
+    """
+    catalog = HeldCatalog()
+    process = _spawn_acp(daimonos_binary, tmp_path, base_url=catalog.url)
+    try:
+        assert catalog.arrived.wait(timeout=10), "startup did not request the model catalog"
+        started = time.monotonic()
+        process.send_signal(sig)
+        # The catalog reply is still held: the exit does not wait for it.
+        assert process.wait(timeout=4) == 0
+        assert time.monotonic() - started < 4
+        assert not catalog.release.is_set()
+    finally:
+        _stop(process)
+        catalog.close()
diff --git a/tests/test_agent_env_precedence.py b/tests/test_agent_env_precedence.py
new file mode 100644
index 0000000..9df3ede
--- /dev/null
+++ b/tests/test_agent_env_precedence.py
@@ -0,0 +1,241 @@
+"""Precedence of agent settings: real environment > `--agent-env` file > implicit files.
+
+The implicit files are `<workspace>/agent.env` and `~/.config/daimonos/agent.env`.
+Daimonos loads them into the process environment at startup. Before vikunja
+#1623 their values then counted as process values and replaced the values of
+the file that `--agent-env` names.
+
+Each test uses two fake provider endpoints. The endpoint that receives the
+model request shows which file supplied `DAIMONOS_AGENT_BASE_URL`.
+"""
+import json
+import os
+import pty
+import signal
+import subprocess
+import threading
+import time
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
+
+from test_agent_oneshot_sessions import Harness, fake_provider, only_json_document, text_reply
+
+__all__ = ["fake_provider"]  # the fixture of the imported module
+
+
+def settings(port, model):
+    return "\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter", f"DAIMONOS_AGENT_MODEL={model}",
+        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{port}", "DAIMONOS_AGENT_API_KEY=test",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_COMPACTION=off", ""])
+
+
+def write_home_agent_env(harness, port, model="home/model"):
+    directory = harness.home / ".config" / "daimonos"
+    directory.mkdir(parents=True)
+    (directory / "agent.env").write_text(settings(port, model))
+
+
+def run(harness, argv, extra_env=None):
+    return subprocess.run(
+        argv, cwd=harness.workspace, env={**harness.environment(), **(extra_env or {})},
+        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
+
+
+def test_explicit_agent_env_wins_over_the_home_agent_env(daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+
+    done = harness.agent("--output", "json", "task")
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE EXPLICIT FILE")
+    assert implicit.requests == [], "the run used the endpoint of the home agent.env"
+    assert len(explicit.requests) == 1
+
+
+def test_explicit_agent_env_wins_over_the_workspace_agent_env_with_a_state_dir(
+        daimonos_binary, tmp_path, fake_provider):
+    # A run with --state-dir does not read the home file. It reads <workspace>/agent.env.
+    implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    harness.config.write_text("[analytics]\nenabled=false\n")
+    (harness.workspace / "agent.env").write_text(settings(implicit.port, "workspace/model"))
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+
+    done = harness.agent("--output", "json", "task", global_args=("--state-dir", str(state)))
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "test/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_a_file_from_the_real_selector_variable_wins_over_the_home_agent_env(
+        daimonos_binary, tmp_path, fake_provider):
+    # DAIMONOS_AGENT_ENV in the real environment selects the file, as --agent-env does.
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE SELECTED FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+
+    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
+            "agent", "--output", "json", "task"]
+    done = run(harness, argv, {"DAIMONOS_AGENT_ENV": str(harness.agent_env)})
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "test/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_a_file_that_the_workspace_agent_env_selects_is_not_an_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    # Unchanged behavior: an implicit file can name the agent env file. It then
+    # keeps its rank above that file, so its own values win.
+    implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
+    selected = fake_provider([text_reply("FROM THE SELECTED FILE")])
+    harness = Harness(daimonos_binary, tmp_path, selected.port)
+    (harness.workspace / "agent.env").write_text(
+        f"DAIMONOS_AGENT_ENV={harness.agent_env}\n"
+        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{implicit.port}\n")
+
+    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
+            "agent", "--output", "json", "task"]
+    done = run(harness, argv)
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE WORKSPACE FILE")
+    assert selected.requests == []
+
+
+def test_the_real_environment_wins_over_the_explicit_agent_env(daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+
+    done = run(harness, harness.command("--output", "json", "task"),
+               {"DAIMONOS_AGENT_MODEL": "environment/model"})
+    assert done.returncode == 0, done.stderr
+    # The model comes from the environment. The endpoint comes from the explicit file.
+    assert only_json_document(done.stdout)["model"] == "environment/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_the_home_agent_env_supplies_a_value_that_the_explicit_file_omits(
+        daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+    lines = [line for line in harness.agent_env.read_text().splitlines()
+             if not line.startswith("DAIMONOS_AGENT_MODEL=")]
+    harness.agent_env.write_text("\n".join(lines) + "\n")
+
+    done = harness.agent("--output", "json", "task")
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "home/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_the_home_agent_env_is_the_selected_file_without_an_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    # Control: without --agent-env nothing changes. The home file is the selected file.
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    harness = Harness(daimonos_binary, tmp_path, 1)
+    write_home_agent_env(harness, implicit.port)
+
+    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
+            "agent", "--output", "json", "task"]
+    done = run(harness, argv)
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["model"], doc["final_text"]) == ("home/model", "FROM THE HOME FILE")
+    assert len(implicit.requests) == 1
+
+
+class CatalogEndpoint:
+    """A provider endpoint that answers the model-catalog request and counts it."""
+
+    def __init__(self):
+        owner = self
+        self.requested = threading.Event()
+
+        class Handler(BaseHTTPRequestHandler):
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                owner.requested.set()
+                body = json.dumps({"data": [{"id": "test/model", "context_length": 100000}]}).encode()
+                self.send_response(200)
+                self.send_header("Content-Type", "application/json")
+                self.send_header("Content-Length", str(len(body)))
+                self.end_headers()
+                self.wfile.write(body)
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        threading.Thread(target=self.server.serve_forever, daemon=True).start()
+        self.port = self.server.server_address[1]
+
+    def close(self):
+        self.server.shutdown()
+        self.server.server_close()
+
+
+def test_the_session_daemon_of_an_interactive_run_uses_the_explicit_agent_env(daimonos_binary, tmp_path):
+    """`agent --interactive` starts the session daemon as a child process. The
+    daemon asks its provider for the model catalog at startup. That request
+    must go to the endpoint of the `--agent-env` file, not to the endpoint of
+    the home `agent.env` that the parent process loaded.
+    """
+    implicit, explicit = CatalogEndpoint(), CatalogEndpoint()
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port, model="test/model")
+    socket_path = tmp_path / "daemon.sock"
+    harness.config.write_text(
+        "[analytics]\nenabled=false\n"
+        f'[session]\nsocket_path = "{socket_path}"\nbootstrap_timeout_secs = 10\n'
+        "bootstrap_retry_interval_ms = 10\n")
+    instance_path = tmp_path / "daemon.sock.pid"
+    master_fd, slave_fd = pty.openpty()
+    process = subprocess.Popen(
+        harness.command("--interactive"), cwd=harness.workspace,
+        env={**harness.environment(), "TERM": "xterm"},
+        stdin=slave_fd, stdout=slave_fd, stderr=subprocess.PIPE, text=True)
+    os.close(slave_fd)
+    try:
+        deadline = time.monotonic() + 15
+        while time.monotonic() < deadline and not (explicit.requested.is_set() or implicit.requested.is_set()):
+            assert process.poll() is None, process.stderr.read()
+            time.sleep(0.02)
+        assert not implicit.requested.is_set(), "the daemon used the endpoint of the home agent.env"
+        assert explicit.requested.is_set(), "the daemon requested no model catalog"
+    finally:
+        if process.poll() is None:
+            process.terminate()
+            try:
+                process.wait(timeout=5)
+            except subprocess.TimeoutExpired:
+                process.kill()
+                process.wait()
+        deadline = time.monotonic() + 10
+        while not instance_path.exists() and time.monotonic() < deadline:
+            time.sleep(0.02)
+        if instance_path.exists():
+            try:
+                os.kill(json.loads(instance_path.read_text())["pid"], signal.SIGTERM)
+            except (ProcessLookupError, ValueError):
+                pass
+            deadline = time.monotonic() + 5
+            while socket_path.exists() and time.monotonic() < deadline:
+                time.sleep(0.02)
+        os.close(master_fd)
+        implicit.close()
+        explicit.close()
+    assert not socket_path.exists(), "the test left a session daemon"

### Known Concerns
1. The record of implicit variables is process-global mutable state (a static Mutex). It is written at startup in main, before the runtime mode dispatch, and read by AgentEnv::load. Rust unit tests in one test binary share it; no unit test writes it.
2. A variable that an implicit file set is `implicit` even if the same name and value also existed... no: env_file never overwrites a variable that the environment already has, so a launcher-supplied variable is never recorded. But a launcher-supplied EMPTY variable (DAIMONOS_AGENT_MODEL='') counts as present for env_file (var_os is Some), so the implicit file does not set it, and merge_process_overrides ignores blank values, so the selected file wins. That is unchanged behavior.
3. Five variables are read directly from the process environment and are not parsed by agent_env (DAIMONOS_AGENT_MODELS_DISCOVER, DAIMONOS_AGENT_MAX_TOKENS, DAIMONOS_AGENT_ERROR_RESUME, DAIMONOS_AGENT_AUTO_CONTINUE, DAIMONOS_AGENT_SESSION_ID). For them an implicit file still applies and the selected file has no effect. ADR-037 lists this under Consequences and Open questions.
4. DAIMONOS_SKILL_DIR is read through agent_env and is covered. Other consumers of implicit-file variables (tool child processes inherit the process environment) still see the implicit values, by design of env_file.
5. The session daemon removal covers one self-spawn. Other child processes that are Daimonos itself (for example an outbound MCP server entry that points to the daimonos binary, or a user who runs `daimonos agent` from a tool call) inherit implicit values as real values. Not changed.
6. If a session daemon is already running (started earlier, possibly by a process with other implicit files), an interactive run attaches to it; the bootstrap fingerprint covers the workspace, config, and agent-env paths, not the content of implicit files. Unchanged by this PR.
7. FIX B changes only `acp`. The session daemon (agent_runtime::run_session_daemon) and the MCP stdio server (mcp.rs) also install their signal handlers after startup work. Not changed.
8. On a signal during acp startup the process returns Ok(()) from run_acp: the startup future is dropped (an in-flight HTTP request is cancelled). The analytics store may be opened inside prepare_acp before the signal; it is dropped without an explicit close.
9. tokio::signal::unix::signal(SignalKind::interrupt()) replaces tokio::signal::ctrl_c() for SIGINT in acp. Both register a SIGINT listener on unix. `acp` is unix-only code already (it uses tokio::signal::unix before this change).
10. tests/test_cli_modes.py and tests/test_cli_prompts.py still start the binary with the real HOME; they were not changed. One of them starts `agent --interactive --agent-env` and so a session daemon, which on a developer host with a home agent.env used to contact the real provider; with FIX A it uses the explicit file.
11. The new daemon process test uses a pty and a real session daemon, and cleans it up by pid file; a failure in the middle could leave a daemon process.

### Specific Questions for Reviewer
1. Is the new precedence implemented as ADR-037 states on every path: agent --print, agent --interactive (parent and daemon), chat, acp, session-daemon started directly, with --state-dir (load_explicit), and with confinement (load_confined, caller_agent_env)?
2. Can a value from an implicit file still override an explicitly selected file, or can a credential from one source be sent to the endpoint of another source, through a path this change missed?
3. Does the change alter behavior for a run WITHOUT an explicit selection, or for a launcher that sets real environment variables (the editor ACP env block)?
4. Is the explicit_selection rule right for $DAIMONOS_AGENT_ENV set by an implicit file, and consistent with resolve_path and load_explicit?
5. FIX B: is there a window left where a signal uses the default action; is exit code 0 during startup correct; can dropping the startup future leave a child process, a lock, or a partial write; does passing the signals value into acp_cmd::run_acp lose a signal that arrived between the two selects?
6. Do the tests prove the claims, and does any test pass for a wrong reason or risk flakiness in CI?
7. Is ADR-037 accurate and complete against the code?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. ADR-037's decision driver states 'No credential of a host file may reach an endpoint that an explicit file names, and the reverse', but the implemented fill rule violates it: with an explicit selection, fill_unset_from_implicit lets an implicit file supply DAIMONOS_AGENT_API_KEY (or DAIMONOS_AGENT_BASE_URL) when the explicit file omits or leaves it blank. An explicit file that sets BASE_URL but omits API_KEY will send the home file's credential to the explicit file's endpoint — exactly the cross-source credential leak the driver forbids. Either the fill rule must exclude credential-bearing settings (at minimum DAIMONOS_AGENT_API_KEY, arguably BASE_URL when the key is filled), or the ADR must explicitly document and justify this exception under Decision/Consequences. The current text and code contradict each other, and the test suite (test_the_home_agent_env_supplies_a_value_that_the_explicit_file_omits) only exercises the benign MODEL case, not the API key.
B2. FIX B leaves a pre-handler signal window and an exit-code inconsistency the proposal does not account for. (a) TerminationSignals::install() runs inside run_acp, which is reached only after main.rs startup work (env_file::load_default, config load, arg parsing, runtime dispatch); a signal in that window still takes the default action and exits by signal. The lifecycle test only sends the signal after the model-catalog request has arrived, so this window is untested and the CHANGELOG claim 'They are now installed first' overstates the fix. At minimum, document the residual window (in the ADR or code comment) and bound it, or move installation earlier. (b) On install() failure run_acp returns Err via `?`, which is a nonzero exit for a condition the user did not cause — acceptable, but the known-concerns list the analytics store being dropped without close when the startup future is dropped; prepare_acp opens analytics before the provider wait, so a signal during the catalog request drops an opened AnalyticsStore mid-flight. Confirm (and state in the ADR/comment) that AnalyticsStore drop is safe with no partial write, e.g. WAL-backed SQLite or an explicit flush on drop; otherwise flush/close it in the signal arm before returning Ok(()).
B3. The explicit_selection check in AgentEnv::load uses std::env::var_os("DAIMONOS_AGENT_ENV").is_some() combined with implicit_keys().contains("DAIMONOS_AGENT_ENV"), but env_file's DENYLIST/handling and the implicit_keys record are consulted at a different time than resolve_path reads the selector, and load_explicit (ADR-021 --state-dir path) bypasses explicit_selection entirely: it calls Self::load_layered? — per the diff, load_explicit is not shown being updated to the new signature. The diff changes load_with_process_vars to load_layered but the load_explicit body (which previously called the old seam) is not in the diff. If load_explicit still calls the old function (compile error) or calls load_layered with explicit=false, the --state-dir path either breaks or silently keeps old precedence, while the confined paths (load_confined, caller_agent_env) named in the request's own question list are also absent from the diff. Please show that every load path (load, load_explicit, load_confined, caller_agent_env) computes `explicit` correctly and splits real/implicit vars, or state explicitly why each other path is unaffected. The pytest state-dir test passes a non-empty explicit file for every setting, so it would not detect explicit=false on that path (the real-env/implicit split alone could make it pass if the workspace file keys are recorded as implicit).

### Non-Blocking Suggestions
S1. fill_unset_from_implicit hardcodes the key-classification predicate (starts_with DAIMONOS_AGENT_, == SKILL_DIR_ENV, != DAIMONOS_AGENT_ENV), but AgentEnv::load already filtered the implicit map to those prefixes — except it does not exclude DAIMONOS_AGENT_ENV from the maps. Consider filtering once at the collection site (and excluding DAIMONOS_AGENT_ENV there) so the predicate cannot drift between the two places; also note that merge_process_overrides in the non-explicit arm receives DAIMONOS_AGENT_ENV in the implicit map, which is harmless today but is an asymmetry worth a comment or removal.
S2. The lifecycle test asserts `time.monotonic() - started < 4` immediately after `process.wait(timeout=4)` succeeded — the second assertion is redundant with the first and can only add flakiness at the boundary; drop it or tighten the wait timeout instead. In the daemon pty test, consider pytest's addfinalizer or a try/finally around the HTTP servers created before Popen (if Popen raises, implicit/explicit servers leak until process exit), and move catalog.close()/server closes into a context manager for symmetry with HeldCatalog.
S3. Known concern 7 notes run_session_daemon and the MCP stdio server still install signal handlers after startup work — the same bug class just fixed for acp. File a follow-up issue (referenced from ADR-037 'Revisit when' or the changelog) so the asymmetry is tracked rather than only recorded in this review.
S4. docs/configuration.md and ADR-037 list the five directly-read variables; consider also stating there that tool child processes still inherit implicit values (known concern 4) and that other self-spawned daimonos children besides the session daemon see implicit values as real (known concern 5), since operators debugging precedence will read configuration.md first.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1623-agent-env-acp-signals/artifacts/round-1-rebuttal.diff" sha256="ab9fa4b564c44cb64969aebc4600cffb4693d4792d69e5c4931c3113fa21df79" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — Correct: the fill rule contradicted the driver that the ADR states. The code is changed, not the driver. agent_env::is_connection_setting names the settings that say where requests go and which credential they carry: DAIMONOS_AGENT_PROVIDER, each *_BASE_URL, and each *_API_KEY, which includes the secondary providers (DAIMONOS_AGENT_<PROVIDER>_API_KEY and ..._BASE_URL). With an explicit selection, fill_unset_from_implicit never uses such a value from an implicit file; it returns their names. The settings then come from the selected file or from the real environment. A run that lacks a required one stops in from_vars before any request, and the error adds 'Not used from an implicit agent.env file: <names>' with no value. Without an explicit selection nothing changes. Tests: Rust an_explicit_file_takes_no_connection_setting_from_an_implicit_file (the predicate for five connection names and three other names; an explicit file without an API key fails and names DAIMONOS_AGENT_API_KEY and DAIMONOS_AGENT_OPENAI_API_KEY and holds no key text; the real environment can supply the key and no secondary provider comes from the implicit file; without an explicit selection the old result holds). Process tests with two endpoints: test_the_api_key_of_the_home_agent_env_is_not_sent_to_the_endpoint_of_the_explicit_file (non-zero exit, no stdout, the setting named on stderr, the key text absent from stderr, zero requests at both endpoints), test_the_endpoint_of_the_home_agent_env_does_not_get_the_api_key_of_the_explicit_file (the reverse), and test_the_real_environment_can_supply_a_connection_setting_for_the_explicit_file. Mutation: with the connection check disabled, the first two process tests fail. ADR-037 states the rule under Decision and its cost under Consequences (a named file that relied on the home file for its key now fails at startup).
Re B2: CLARIFY — (a) Accepted. The residual window is measured and documented, and the changelog no longer says 'installed first'. Measured on the fixed binary with /proc/PID/status (SigCgt bit for SIGTERM), 15 starts each: handlers installed after 13 to 18 ms with the real HOME and 12 to 16 ms with an empty HOME. Before the fix the same measure was 30 ms with an empty HOME, 300 ms to 1 s with a home file, and equal to the provider delay (2.04 s for a 2.0 s catalog). The work in that window is in main: argument parsing, the implicit env files, the configuration, logging. It makes no network request. Moving the install into main before the mode dispatch would cover a few more milliseconds and would put ACP-specific signal state into main for every mode; the comment at the install site states the window. The process cannot remove the window completely: a signal before the runtime exists always uses the default action. (b) The premise is not what the code does: prepare_acp opens the analytics store AFTER the provider waits. Order in prepare_acp: load_agent_env, resolve_compaction (provider), discover_models (provider), safety_policy, verify_session_store, then AnalyticsStore::open_async. So a signal during the catalog request finds no open store. A signal during the open itself: open_async runs AnalyticsStore::new in tokio spawn_blocking; when the startup future is dropped the blocking task runs to its end and its result, the store, is dropped, which closes the SQLite connection. The open does a schema batch and a retention delete on a connection in WAL mode with synchronous=NORMAL; SQLite keeps the file consistent if the process ends in the middle. No explicit flush exists or is needed at that point, because no analytics record of this process was written yet. The comment at the select states this. On install() failure the function returns the error and the process exits non-zero; that is unchanged in kind, because the old code used `?` on the same two registrations.
Re B3: CLARIFY — Each load path goes through the one changed function AgentEnv::load, so each computes `explicit` and splits real and implicit values. The bodies that the diff does not show are unchanged because they only call it. (1) agent_runtime::load_agent_env without --state-dir and without confinement: AgentEnv::load(path). (2) With --state-dir: AgentEnv::load_explicit(path), whose body is `if flag.is_none() && DAIMONOS_AGENT_ENV is unset { return Err } Self::load(flag)`. It was not edited and it compiles, because load keeps its signature; only the private seam load_with_process_vars was replaced by load_layered. (3) With confinement: load_agent_env takes `path.or_else(|| cfg.caller_agent_env.clone())` and calls AgentEnv::load(Some(selected)), so flag is Some and the selection is explicit. cfg.caller_agent_env is DAIMONOS_AGENT_ENV of the caller, read in main before any implicit file is loaded, so it is a value of the real environment. (4) env_file::load_confined and load_default_with_user_config both end in apply_content, which is where the record is written, so the record exists on each of these paths. Proof by test and mutation: with `explicit` forced to false in AgentEnv::load, 6 of the 8 process tests of round 1 fail, including test_explicit_agent_env_wins_over_the_workspace_agent_env_with_a_state_dir, so that test does detect explicit=false on the --state-dir path (the workspace file sets a different endpoint and model, and with explicit=false its values override the explicit file). New process test for the confined path: test_explicit_agent_env_wins_over_the_workspace_agent_env_under_confinement runs `--state-dir --confine-paths agent` with a workspace agent.env that names another endpoint and model, once with --agent-env and once with DAIMONOS_AGENT_ENV in the real environment; both must use the explicit file and send nothing to the other endpoint. On the timing point: implicit_keys and resolve_path are both read inside AgentEnv::load, after main finished env_file loading; nothing writes the record or the selector between those two reads.

### Revised Code / Diff
diff --git a/CHANGELOG.md b/CHANGELOG.md
index fbfc69c..2a21eaf 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -7,6 +7,23 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ## [Unreleased]
 
+### Fixed
+
+- **An explicit `--agent-env` file lost to the home `agent.env`** (ADR-037,
+  vikunja #1623). Daimonos loads `<workspace>/agent.env` and
+  `~/.config/daimonos/agent.env` into the process environment at startup, and
+  their values then replaced the values of the file that `--agent-env` or
+  `$DAIMONOS_AGENT_ENV` names. A run with `--agent-env` on a host with a home
+  `agent.env` used the provider endpoint, the model, and the API key of the
+  home file. The order is now: real environment, then the explicitly selected
+  file, then the implicit files. With an explicitly selected file, the
+  connection settings (provider, base URL, API key) never come from an implicit
+  file. A run without an explicit selection does not change.
+- **A signal during `daimonos acp` startup killed the process** (vikunja
+  #1623). The handlers for SIGTERM, SIGINT, and SIGHUP were installed after the
+  startup requests to the provider. They are now installed before those
+  requests, and a signal during them ends the process with exit code 0.
+
 ### Changed
 
 - **Releases start from CI.** A merge to `master` that changes the version in
diff --git a/docs/adr/037-explicit-agent-env-ranks-above-implicit-files.md b/docs/adr/037-explicit-agent-env-ranks-above-implicit-files.md
new file mode 100644
index 0000000..3a94650
--- /dev/null
+++ b/docs/adr/037-explicit-agent-env-ranks-above-implicit-files.md
@@ -0,0 +1,88 @@
+# ADR-037: An explicitly selected agent env file ranks above the implicit `agent.env` files
+
+- **Status:** Accepted
+- **Date:** 2026-11-23
+- **Deciders:** Repository maintainer, by explicit user decision on 2026-11-23 ("do all three", after the options were shown).
+- **Consulted:** None recorded.
+- **Informed:** None recorded.
+- **Spec:** Vikunja #1623.
+- **Dependencies:** ADR-021 (state root: no home `agent.env`), ADR-022 (confinement: workspace `agent.env` only).
+- **Supersedes:** None
+- **Superseded by:** None
+
+## Context
+
+Two mechanisms read `agent.env` files.
+
+- `env_file` runs first, for each runtime mode. It reads the **implicit** files `<workspace>/agent.env` and `~/.config/daimonos/agent.env` and sets each variable that the process environment does not have.
+- `agent_env` then reads the **selected** file for `agent`, `chat`, `acp`, and the session daemon. The selection is `--agent-env`, else `$DAIMONOS_AGENT_ENV`, else the home file. Non-empty process values named `DAIMONOS_AGENT_*` override the values of the selected file.
+
+The second step cannot tell a value of the launcher from a value that the first step set. So a value of an implicit file replaced the value of the file that the caller named with `--agent-env`.
+
+Measured on 2026-11-23: with a home `agent.env`, a run with `--agent-env <file>` sent its requests to the endpoint, with the model and the API key, of the home file. An unattended caller that passes `--agent-env` (an orchestrator, a test) got the settings of the developer host.
+
+How must an explicitly selected agent env file rank against the implicit files?
+
+## Decision drivers
+
+- A caller that names a file must get the settings of that file.
+- A launcher that sets a variable in the real environment must keep its override (the ACP `env` block of an editor).
+- A run without an explicit selection must not change.
+- No credential of a host file may reach an endpoint that an explicit file names, and the reverse.
+
+## Decision
+
+We will rank the settings of an agent run in this order, highest first:
+
+1. CLI behavior options, for example `--model`.
+2. The real environment: a non-empty `DAIMONOS_AGENT_*` variable that the launcher supplied.
+3. The explicitly selected file.
+4. The implicit `agent.env` files.
+
+A file is explicitly selected when `--agent-env` names it, or when `$DAIMONOS_AGENT_ENV` of the real environment names it. A selector that an implicit file supplies is not an explicit selection.
+
+- `env_file` records the name of each variable that it sets (`env_file::implicit_keys`). `agent_env` uses that record to split the process values into real values and implicit values.
+- With an explicit selection, an implicit value only fills a setting that the selected file omits or leaves blank.
+- With an explicit selection, an implicit value never fills a **connection setting**: `DAIMONOS_AGENT_PROVIDER`, each `..._BASE_URL`, and each `..._API_KEY`, also of a secondary provider. These come from the selected file or from the real environment. So the API key of one file is never sent to the endpoint that another file names. A run that lacks a required connection setting stops before a request, and the message names the settings that were not used. The message does not hold a value.
+- Without an explicit selection, the behavior does not change. The selected file is then the home file, and each process value overrides it.
+- The session daemon does not inherit the implicit values of the process that starts it. It loads the implicit files itself, so it has its own record.
+- The rule covers the settings that `agent_env` parses: `DAIMONOS_AGENT_*` and `DAIMONOS_SKILL_DIR`.
+
+## Alternatives
+
+- **Do not load the implicit files when a file is explicitly selected.** This is the cleanest isolation. It loses: an implicit file also supplies variables that `agent_env` does not parse (see Consequences) and variables for tool child processes. The loss of those would change more runs.
+- **Let the explicit file win over the real environment too.** It loses: an editor that starts `acp` with an `env` block and an `--agent-env` option would lose its override, against the rule of `env_file` that the launcher wins.
+- **Keep the order and change the documents.** It loses: a named file that a host file overrides cannot give a reproducible run.
+- **Clear the environment in each caller.** It works for a test. It loses as a product rule: each caller must know the rule, and a caller with a home file that omits it sends credentials to the wrong endpoint.
+
+## Consequences
+
+- A run with `--agent-env` on a host with a home `agent.env` changes: the named file now wins for each setting that it sets. This is the intended change.
+- A setting that the named file omits still comes from an implicit file, except a connection setting. A caller that needs a closed set of settings uses `--state-dir`, which reads no home file (ADR-021).
+- A named file that relied on the home file for its API key or base URL now fails at startup. The fix is to put the setting into the named file or into the real environment.
+- Without an explicit selection, nothing changes, including the order of the two implicit files. This decision does not cover that order (Vikunja #1624).
+- Tool child processes inherit the process environment, with the values of the implicit files, as before.
+- Not covered: variables that the product reads directly from the process environment and that `agent_env` does not parse. They are `DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`, `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, and `DAIMONOS_AGENT_SESSION_ID`. For these, a line in the selected file has no effect, as before, and a value of an implicit file or of the real environment applies.
+- The record of implicit variables is process state. A process that another Daimonos process starts, and that inherits its environment, sees the implicit values as real values. The session daemon is the one such child, and its start removes them.
+
+## Confirmation
+
+- `pytest tests/test_agent_env_precedence.py` drives the real binary with two provider endpoints. It covers: the explicit file against the home file, against the workspace file with `--state-dir`, the real environment above the explicit file, an omitted setting filled from the home file, `$DAIMONOS_AGENT_ENV` of the real environment, a selector from an implicit file, the unchanged run without a selection, and the session daemon of an interactive run.
+- The same file covers the connection settings: an explicit file without an API key does not get the key of the home file, an explicit file without a base URL does not use the endpoint of the home file, the real environment can supply either, and the confined run with both ways to name the file.
+- `cargo test agent_env::` covers the order in `layered_vars`, the fill rule, `is_connection_setting`, and `explicit_selection`.
+- `cargo test daemon_command_does_not_pass_on_implicit_agent_env_values` covers the environment of the session daemon.
+
+## Anchors
+
+_Anchors:_ src/env_file.rs::implicit_keys, src/agent_env.rs::AgentEnv::load, src/agent_env.rs::layered_vars, src/agent_env.rs::explicit_selection, src/agent_env.rs::fill_unset_from_implicit, src/agent_env.rs::is_connection_setting, src/session_bootstrap.rs::daemon_command
+
+## Revisit when
+
+A second Daimonos child process inherits the environment of its parent and reads agent settings, or a caller needs an explicit file that reads no implicit file without `--state-dir`.
+
+The ACP signal change of the same pull request is not a part of this decision. The session daemon and the MCP stdio server still install their signal handlers after their startup work (Vikunja #1625).
+
+## Open questions
+
+1. Should the five directly read variables in Consequences move into `agent_env`, so that the selected file can set them? Not decided.
+2. The rules for a run without an explicitly selected file are outside this decision. Vikunja #1624 tracks them.
diff --git a/docs/configuration.md b/docs/configuration.md
index 71c9eb1..6097f2e 100644
--- a/docs/configuration.md
+++ b/docs/configuration.md
@@ -121,6 +121,41 @@ CLI behavior flags (for example --model) > process DAIMONOS_AGENT_* values
 > selected agent.env file values
 ```
 
+Daimonos also loads two **implicit** files into the process environment at
+startup: `<workspace>/agent.env`, then `~/.config/daimonos/agent.env`. They set
+only variables that the environment does not have. With `--state-dir` the home
+file is not read. With confinement only the workspace file is read.
+
+When the file is selected explicitly, with `--agent-env` or with
+`$DAIMONOS_AGENT_ENV` in the real environment, a value from an implicit file
+ranks below the selected file (ADR-037):
+
+```text
+CLI behavior flags > real environment DAIMONOS_AGENT_* values
+> explicitly selected agent.env file > implicit agent.env files
+```
+
+- A value from an implicit file fills only a setting that the selected file
+  omits or leaves blank.
+- A value from an implicit file never fills a connection setting:
+  `DAIMONOS_AGENT_PROVIDER`, each `..._BASE_URL`, and each `..._API_KEY`. Put
+  these into the selected file or into the real environment. A run that lacks
+  one stops at startup and names it.
+- Without an explicit selection, nothing changes: the selected file is the home
+  file, and each process value overrides it.
+- A selector that an implicit file sets (`DAIMONOS_AGENT_ENV=...` in
+  `<workspace>/agent.env`) is not an explicit selection.
+- The rule covers `DAIMONOS_AGENT_*` settings that the agent env file defines,
+  and `DAIMONOS_SKILL_DIR`. Five variables are read from the process
+  environment only, so a line in the selected file has no effect on them:
+  `DAIMONOS_AGENT_MODELS_DISCOVER`, `DAIMONOS_AGENT_MAX_TOKENS`,
+  `DAIMONOS_AGENT_ERROR_RESUME`, `DAIMONOS_AGENT_AUTO_CONTINUE`, and
+  `DAIMONOS_AGENT_SESSION_ID`.
+- Tool child processes inherit the process environment, with the values of the
+  implicit files. A Daimonos process that a tool starts sees those values as
+  values of the real environment. The session daemon of `agent --interactive`
+  does not: it loads the implicit files itself.
+
 Required effective values remain
 `DAIMONOS_AGENT_PROVIDER`, `DAIMONOS_AGENT_MODEL`,
 `DAIMONOS_AGENT_BASE_URL`, `DAIMONOS_AGENT_APPROVAL_MODE`,
diff --git a/src/acp_cmd.rs b/src/acp_cmd.rs
index 748690d..21bdf93 100644
--- a/src/acp_cmd.rs
+++ b/src/acp_cmd.rs
@@ -3879,6 +3879,38 @@ fn build_agent_with_state(
         )
 }
 
+/// The signals that end the ACP engine: SIGTERM, SIGHUP, and SIGINT.
+///
+/// The caller installs them before the startup work that waits for the
+/// provider (model discovery), so a signal during startup does not use the
+/// default action (vikunja #1623). A signal that arrives after `install` is
+/// kept until the next `recv`.
+pub struct TerminationSignals {
+    terminate: tokio::signal::unix::Signal,
+    hangup: tokio::signal::unix::Signal,
+    interrupt: tokio::signal::unix::Signal,
+}
+
+impl TerminationSignals {
+    pub fn install() -> std::io::Result<Self> {
+        use tokio::signal::unix::{signal, SignalKind};
+        Ok(Self {
+            terminate: signal(SignalKind::terminate())?,
+            hangup: signal(SignalKind::hangup())?,
+            interrupt: signal(SignalKind::interrupt())?,
+        })
+    }
+
+    /// Wait for one of the signals and return its name.
+    pub async fn recv(&mut self) -> &'static str {
+        tokio::select! {
+            _ = self.terminate.recv() => "SIGTERM",
+            _ = self.hangup.recv() => "SIGHUP",
+            _ = self.interrupt.recv() => "SIGINT",
+        }
+    }
+}
+
 /// Run the `daimonos acp` engine to completion (until stdin closes).
 /// `make_provider` builds a fresh provider per session (Zed keeps one
 /// process across chat threads); it's validated once up front so a
@@ -3897,6 +3929,7 @@ pub async fn run_acp(
     analytics: Option<Arc<AnalyticsStore>>,
     timestamp_turns: bool,
     thinking: crate::providers::ThinkingLevel,
+    mut signals: TerminationSignals,
 ) -> anyhow::Result<()> {
     tracing::info!(
         target: "daimonos::acp",
@@ -3973,14 +4006,10 @@ pub async fn run_acp(
     tokio::pin!(eof_wait);
     // A client that stops the engine by signal gets the same teardown as EOF;
     // the default handlers would orphan running tool process groups (#1544).
-    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
-    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
     let (result, input_error, signal) = tokio::select! {
         result = &mut connection => (Some(result), None, None),
         input_error = &mut eof_wait => (None, input_error, None),
-        _ = tokio::signal::ctrl_c() => (None, None, Some("SIGINT")),
-        _ = terminate.recv() => (None, None, Some("SIGTERM")),
-        _ = hangup.recv() => (None, None, Some("SIGHUP")),
+        signal = signals.recv() => (None, None, Some(signal)),
     };
     tracing::info!(
         target: "daimonos::acp",
diff --git a/src/agent_env.rs b/src/agent_env.rs
index 469b287..1bce681 100644
--- a/src/agent_env.rs
+++ b/src/agent_env.rs
@@ -11,6 +11,11 @@
 //!   `--agent-env <path>`  >  `$DAIMONOS_AGENT_ENV`  >  `~/.config/daimonos/agent.env`
 //! Value precedence after selecting that file:
 //!   non-empty process `$DAIMONOS_AGENT_*`  >  value parsed from the file
+//!
+//! With an explicitly selected file (`--agent-env` or `$DAIMONOS_AGENT_ENV`),
+//! a process value that an implicit `agent.env` file supplied ranks below the
+//! selected file (ADR-037, `env_file::implicit_keys`):
+//!   real environment  >  selected file  >  implicit `agent.env` files
 
 use std::collections::HashMap;
 use std::path::{Path, PathBuf};
@@ -142,16 +147,30 @@ impl AgentEnv {
     pub fn load(flag: Option<PathBuf>) -> Result<AgentEnv, String> {
         // Use vars_os so an unrelated non-Unicode process variable cannot
         // panic agent startup. Non-Unicode agent keys/values are ignored.
-        let process_vars: HashMap<String, String> = std::env::vars_os()
-            .filter_map(|(key, value)| {
-                let key = key.into_string().ok()?;
-                if !key.starts_with("DAIMONOS_AGENT_") && key != crate::skills::SKILL_DIR_ENV {
-                    return None;
-                }
-                Some((key, value.into_string().ok()?))
-            })
-            .collect();
-        Self::load_with_process_vars(flag, &process_vars)
+        let implicit_keys = crate::env_file::implicit_keys();
+        let mut real = HashMap::new();
+        let mut implicit = HashMap::new();
+        for (key, value) in std::env::vars_os() {
+            let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) else {
+                continue;
+            };
+            // The selector `DAIMONOS_AGENT_ENV` is not a setting. It is read
+            // below, for the selection.
+            if !is_agent_setting(&key) {
+                continue;
+            }
+            if implicit_keys.contains(&key) {
+                implicit.insert(key, value);
+            } else {
+                real.insert(key, value);
+            }
+        }
+        let explicit = explicit_selection(
+            flag.is_some(),
+            std::env::var_os("DAIMONOS_AGENT_ENV").is_some(),
+            implicit_keys.contains("DAIMONOS_AGENT_ENV"),
+        );
+        Self::load_layered(flag, explicit, &real, &implicit)
     }
 
     /// Load only an explicitly selected agent env file under ADR-021.
@@ -164,9 +183,18 @@ impl AgentEnv {
 
     /// Injectable load seam used by tests to verify real file + process
     /// precedence without mutating the process-global environment.
-    fn load_with_process_vars(
+    ///
+    /// `real` holds the process values that the launcher supplied. `implicit`
+    /// holds the process values that an implicit `agent.env` file supplied.
+    /// ADR-037: with an explicitly selected file (`explicit`), an implicit
+    /// value only fills a setting that the selected file does not set, and
+    /// never a connection setting. Without one, both kinds override the
+    /// selected file as before.
+    fn load_layered(
         flag: Option<PathBuf>,
-        process_vars: &HashMap<String, String>,
+        explicit: bool,
+        real: &HashMap<String, String>,
+        implicit: &HashMap<String, String>,
     ) -> Result<AgentEnv, String> {
         let path = Self::resolve_path(flag).ok_or_else(|| {
             "cannot resolve agent env file path — set --agent-env or $DAIMONOS_AGENT_ENV, \
@@ -181,9 +209,18 @@ impl AgentEnv {
                 REQUIRED.join(", ")
             )
         })?;
-        let mut vars = parse_dotenv(&content);
-        merge_process_overrides(&mut vars, process_vars);
-        Self::from_vars(&vars, &path)
+        let (vars, withheld) = layered_vars(&content, explicit, real, implicit);
+        Self::from_vars(&vars, &path).map_err(|error| {
+            if withheld.is_empty() {
+                return error;
+            }
+            format!(
+                "{error}\nNot used from an implicit agent.env file: {}. With an explicitly \
+                 selected agent env file, the connection settings (provider, base URL, API \
+                 key) come from that file or from the real environment.",
+                withheld.join(", ")
+            )
+        })
     }
 
     fn from_vars(vars: &HashMap<String, String>, path: &Path) -> Result<AgentEnv, String> {
@@ -558,15 +595,89 @@ fn merge_process_overrides(
     process_vars: &HashMap<String, String>,
 ) {
     for (key, value) in process_vars {
-        if (key.starts_with("DAIMONOS_AGENT_") || key == crate::skills::SKILL_DIR_ENV)
-            && key != "DAIMONOS_AGENT_ENV"
-            && !value.trim().is_empty()
-        {
+        if is_agent_setting(key) && !value.trim().is_empty() {
             file_vars.insert(key.clone(), value.clone());
         }
     }
 }
 
+/// ADR-037: the caller selected the agent env file explicitly. That is so with
+/// `--agent-env`, and with `$DAIMONOS_AGENT_ENV` from the real environment. A
+/// selector that an implicit `agent.env` file supplied is not an explicit
+/// selection: that file then keeps its rank above the file that it selects.
+fn explicit_selection(flag_given: bool, selector_set: bool, selector_is_implicit: bool) -> bool {
+    flag_given || (selector_set && !selector_is_implicit)
+}
+
+/// A setting of the agent env file: `DAIMONOS_AGENT_*` or the skill root. The
+/// selector `DAIMONOS_AGENT_ENV` chooses the file and is not a setting.
+fn is_agent_setting(key: &str) -> bool {
+    (key.starts_with("DAIMONOS_AGENT_") || key == crate::skills::SKILL_DIR_ENV)
+        && key != "DAIMONOS_AGENT_ENV"
+}
+
+/// A setting that names where requests go or which credential they carry: the
+/// provider, each base URL, and each API key, also of a secondary provider
+/// (`DAIMONOS_AGENT_<PROVIDER>_API_KEY`, `..._BASE_URL`).
+///
+/// ADR-037: with an explicitly selected file, these never come from an
+/// implicit file. Otherwise the API key of a host file could go to the
+/// endpoint that the selected file names, or the reverse.
+fn is_connection_setting(key: &str) -> bool {
+    key == "DAIMONOS_AGENT_PROVIDER" || key.ends_with("_API_KEY") || key.ends_with("_BASE_URL")
+}
+
+/// The effective settings: the selected file, then the two kinds of process
+/// values in the order of ADR-037. Also returns the names of the connection
+/// settings of implicit files that were not used. Pure, so tests need no
+/// process environment.
+fn layered_vars(
+    content: &str,
+    explicit: bool,
+    real: &HashMap<String, String>,
+    implicit: &HashMap<String, String>,
+) -> (HashMap<String, String>, Vec<String>) {
+    let mut vars = parse_dotenv(content);
+    let mut withheld = Vec::new();
+    if explicit {
+        withheld = fill_unset_from_implicit(&mut vars, implicit);
+    } else {
+        merge_process_overrides(&mut vars, implicit);
+    }
+    merge_process_overrides(&mut vars, real);
+    // A setting that the real environment supplied was not needed from the
+    // implicit file, so it is not reported.
+    withheld.retain(|key| vars.get(key).is_none_or(|value| value.trim().is_empty()));
+    (vars, withheld)
+}
+
+/// Use a value from an implicit `agent.env` file only for a setting that the
+/// explicitly selected file does not set (ADR-037). A blank value in the
+/// selected file does not set the setting, as in [`merge_process_overrides`].
+/// A connection setting is never used ([`is_connection_setting`]). Returns the
+/// sorted names of the connection settings that were not used for that reason.
+fn fill_unset_from_implicit(
+    file_vars: &mut HashMap<String, String>,
+    implicit: &HashMap<String, String>,
+) -> Vec<String> {
+    let mut withheld = Vec::new();
+    for (key, value) in implicit {
+        let unset_in_file = file_vars
+            .get(key)
+            .is_none_or(|existing| existing.trim().is_empty());
+        if !is_agent_setting(key) || !unset_in_file || value.trim().is_empty() {
+            continue;
+        }
+        if is_connection_setting(key) {
+            withheld.push(key.clone());
+        } else {
+            file_vars.insert(key.clone(), value.clone());
+        }
+    }
+    withheld.sort();
+    withheld
+}
+
 /// Build the picker's candidate model list: `current` is always present
 /// (prepended if the configured list omits it), duplicates removed while
 /// preserving first-seen order. Result is never empty.
@@ -793,11 +904,145 @@ mod tests {
             "anthropic/claude-opus-4.8".to_string(),
         )]);
 
-        let env = AgentEnv::load_with_process_vars(Some(path), &process_vars).unwrap();
+        let env = AgentEnv::load_layered(Some(path), true, &process_vars, &HashMap::new()).unwrap();
 
         assert_eq!(env.model, "anthropic/claude-opus-4.8");
     }
 
+    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
+        pairs
+            .iter()
+            .map(|(key, value)| (key.to_string(), value.to_string()))
+            .collect()
+    }
+
+    /// ADR-037: real environment > explicitly selected file > implicit files.
+    #[test]
+    fn an_explicit_file_ranks_between_the_real_environment_and_implicit_values() {
+        let dir = tempfile::tempdir().unwrap();
+        let path = dir.path().join("agent.env");
+        // The file sets the model, leaves the summary model blank, and omits
+        // the thinking level.
+        std::fs::write(&path, format!("{}DAIMONOS_AGENT_SUMMARY_MODEL=\n", base())).unwrap();
+        let implicit = vars(&[
+            ("DAIMONOS_AGENT_MODEL", "implicit/model"),
+            ("DAIMONOS_AGENT_SUMMARY_MODEL", "implicit/summary"),
+            ("DAIMONOS_AGENT_THINKING", "high"),
+            ("DAIMONOS_AGENT_APPROVAL_MODE", "  "),
+            ("DAIMONOS_AGENT_ENV", "/implicit/selector"),
+        ]);
+
+        // The explicit file wins for a setting that it sets.
+        let env =
+            AgentEnv::load_layered(Some(path.clone()), true, &HashMap::new(), &implicit).unwrap();
+        assert_eq!(env.model, "anthropic/claude-sonnet-4.6");
+        // An implicit value fills a setting that the file omits or leaves blank.
+        assert_eq!(env.thinking, ThinkingLevel::High);
+        let content = std::fs::read_to_string(&path).unwrap();
+        let (layered, withheld) = layered_vars(&content, true, &HashMap::new(), &implicit);
+        assert_eq!(layered["DAIMONOS_AGENT_SUMMARY_MODEL"], "implicit/summary");
+        assert_eq!(layered["DAIMONOS_AGENT_APPROVAL_MODE"], "interactive");
+        assert!(!layered.contains_key("DAIMONOS_AGENT_ENV"));
+        assert!(withheld.is_empty());
+
+        // The real environment wins over the explicit file.
+        let real = vars(&[("DAIMONOS_AGENT_MODEL", "real/model")]);
+        let env = AgentEnv::load_layered(Some(path), true, &real, &implicit).unwrap();
+        assert_eq!(env.model, "real/model");
+        assert_eq!(env.thinking, ThinkingLevel::High);
+    }
+
+    /// ADR-037: with an explicitly selected file, no connection setting comes
+    /// from an implicit file. So the API key of a host file cannot go to the
+    /// endpoint that the selected file names, or the reverse.
+    #[test]
+    fn an_explicit_file_takes_no_connection_setting_from_an_implicit_file() {
+        for key in [
+            "DAIMONOS_AGENT_PROVIDER",
+            "DAIMONOS_AGENT_BASE_URL",
+            "DAIMONOS_AGENT_API_KEY",
+            "DAIMONOS_AGENT_OPENAI_API_KEY",
+            "DAIMONOS_AGENT_ANTHROPIC_BASE_URL",
+        ] {
+            assert!(is_connection_setting(key), "{key}");
+        }
+        for key in [
+            "DAIMONOS_AGENT_MODEL",
+            "DAIMONOS_AGENT_APPROVAL_MODE",
+            "DAIMONOS_SKILL_DIR",
+        ] {
+            assert!(!is_connection_setting(key), "{key}");
+        }
+
+        let dir = tempfile::tempdir().unwrap();
+        let path = dir.path().join("agent.env");
+        // The selected file names an endpoint and has no API key.
+        std::fs::write(
+            &path,
+            base().replace("DAIMONOS_AGENT_API_KEY=sk-test\n", ""),
+        )
+        .unwrap();
+        let implicit = vars(&[
+            ("DAIMONOS_AGENT_API_KEY", "sk-host-secret"),
+            ("DAIMONOS_AGENT_OPENAI_API_KEY", "sk-host-openai"),
+            ("DAIMONOS_AGENT_MODEL", "implicit/model"),
+        ]);
+
+        let error = AgentEnv::load_layered(Some(path.clone()), true, &HashMap::new(), &implicit)
+            .unwrap_err();
+        assert!(error.contains("missing required value"), "{error}");
+        assert!(
+            error.contains(
+                "Not used from an implicit agent.env file: \
+                 DAIMONOS_AGENT_API_KEY, DAIMONOS_AGENT_OPENAI_API_KEY"
+            ),
+            "{error}"
+        );
+        assert!(
+            !error.contains("sk-host"),
+            "the message must not hold a key"
+        );
+
+        // The real environment can supply the key. The host key is not used,
+        // and no secondary provider comes from the implicit file.
+        let real = vars(&[("DAIMONOS_AGENT_API_KEY", "sk-real")]);
+        let env = AgentEnv::load_layered(Some(path.clone()), true, &real, &implicit).unwrap();
+        assert_eq!(env.api_key, "sk-real");
+        assert!(env.secondary_providers.is_empty());
+
+        // Without an explicit selection the behavior does not change.
+        let env = AgentEnv::load_layered(Some(path), false, &HashMap::new(), &implicit).unwrap();
+        assert_eq!(env.api_key, "sk-host-secret");
+        assert_eq!(env.secondary_providers.len(), 1);
+    }
+
+    /// Without an explicit selection, the selected file is the home file, and
+    /// an implicit value overrides it as before ADR-037.
+    #[test]
+    fn without_an_explicit_selection_implicit_values_override_the_file() {
+        let implicit = vars(&[("DAIMONOS_AGENT_MODEL", "implicit/model")]);
+        let real = vars(&[("DAIMONOS_AGENT_API_KEY", "sk-real")]);
+        let model = |explicit: bool| {
+            let (layered, _) = layered_vars(&base(), explicit, &real, &implicit);
+            assert_eq!(layered["DAIMONOS_AGENT_API_KEY"], "sk-real");
+            layered["DAIMONOS_AGENT_MODEL"].clone()
+        };
+        assert_eq!(model(false), "implicit/model");
+        assert_eq!(model(true), "anthropic/claude-sonnet-4.6");
+    }
+
+    /// `--agent-env` and `$DAIMONOS_AGENT_ENV` of the real environment are
+    /// explicit selections. The same selector from an implicit file is not.
+    #[test]
+    fn only_the_option_and_the_real_selector_are_explicit_selections() {
+        // (flag given, selector set, selector is implicit) -> explicit
+        assert!(explicit_selection(true, false, false));
+        assert!(explicit_selection(true, true, true));
+        assert!(explicit_selection(false, true, false));
+        assert!(!explicit_selection(false, true, true));
+        assert!(!explicit_selection(false, false, false));
+    }
+
     #[test]
     fn empty_process_value_does_not_override_file() {
         let env = load_str_with_overrides(
diff --git a/src/agent_runtime.rs b/src/agent_runtime.rs
index dad0eac..ec6eb35 100644
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@ -640,6 +640,66 @@ pub async fn run_acp(
     cfg: Arc<config::Config>,
     token_log: Option<PathBuf>,
 ) -> anyhow::Result<()> {
+    // Install the handlers before the startup work that waits for the
+    // provider. A signal during startup then ends the process with code 0.
+    // No session and no tool process exists at that time, so no teardown is
+    // necessary (vikunja #1623).
+    //
+    // Residual window: `main` loads the configuration and starts logging
+    // before it calls this function. A signal in that time uses the default
+    // action. That work makes no network request.
+    //
+    // A signal drops `prepare_acp` at an await point. Its steps are file reads,
+    // provider requests, and at the end the open of the analytics store. That
+    // open runs as a blocking task, which runs to its end when the future is
+    // dropped. SQLite keeps the database consistent in each case.
+    let mut signals = acp_cmd::TerminationSignals::install()?;
+    let startup = tokio::select! {
+        startup = prepare_acp(args, cfg) => startup?,
+        signal = signals.recv() => {
+            tracing::info!(
+                target: "daimonos::acp",
+                event = "acp_stopped_during_startup",
+                signal,
+            );
+            return Ok(());
+        }
+    };
+    acp_cmd::run_acp(
+        startup.make_provider,
+        workspace,
+        startup.cfg,
+        startup.model,
+        startup.models,
+        startup.safety,
+        token_log,
+        startup.sessions_dir,
+        startup.compaction,
+        startup.analytics,
+        startup.timestamp_turns,
+        startup.thinking,
+        signals,
+    )
+    .await
+}
+
+/// What `acp` prepares before it serves its client.
+struct AcpStartup {
+    make_provider: acp_cmd::ProviderFactory,
+    cfg: Arc<config::Config>,
+    model: String,
+    models: Vec<String>,
+    safety: safety::SafetyPolicy,
+    sessions_dir: Option<PathBuf>,
+    compaction: crate::session_core::SessionCompaction,
+    analytics: Option<Arc<analytics::AnalyticsStore>>,
+    timestamp_turns: bool,
+    thinking: providers::ThinkingLevel,
+}
+
+/// The startup work of `acp`. It waits for the provider: the context window
+/// and the model catalog.
+async fn prepare_acp(args: AcpArgs, cfg: Arc<config::Config>) -> anyhow::Result<AcpStartup> {
     let AcpArgs {
         model,
         provider,
@@ -709,21 +769,21 @@ pub async fn run_acp(
     } else {
         None
     };
-    acp_cmd::run_acp(
+    Ok(AcpStartup {
         make_provider,
-        workspace,
         cfg,
-        effective_model,
+        model: effective_model,
         models,
         safety,
-        token_log,
         sessions_dir,
-        crate::session_core::SessionCompaction::new(compaction, compaction_follows_model),
-        analytics_store,
-        agent.timestamp_turns,
-        agent.thinking.clone(),
-    )
-    .await
+        compaction: crate::session_core::SessionCompaction::new(
+            compaction,
+            compaction_follows_model,
+        ),
+        analytics: analytics_store,
+        timestamp_turns: agent.timestamp_turns,
+        thinking: agent.thinking.clone(),
+    })
 }
 
 pub async fn run_session_daemon(
diff --git a/src/env_file.rs b/src/env_file.rs
index 23d5a84..2f33a0f 100644
--- a/src/env_file.rs
+++ b/src/env_file.rs
@@ -11,6 +11,11 @@
 //! A variable already present in the process environment is never overwritten,
 //! so values injected by the launcher (e.g. Zed's ACP `env` block) always win.
 //!
+//! The files of this module are the **implicit** files: no option selects
+//! them. [`implicit_keys`] names the variables that this process set from
+//! them. The agent frontends use it to rank an explicitly selected agent env
+//! file above these values (ADR-037, `agent_env`).
+//!
 //! ## Security
 //! `agent.env` typically lives in a checked-out repository — untrusted input —
 //! and daimonos' environment is inherited by every tool it exec's. To keep a
@@ -18,7 +23,22 @@
 //! that steer how executables/interpreters load are refused from the file (see
 //! [`DENYLIST`]); set those in the real environment if you truly need them.
 
+use std::collections::BTreeSet;
 use std::path::Path;
+use std::sync::Mutex;
+
+/// The variables that this process set from an implicit `agent.env` file.
+static IMPLICIT_KEYS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
+
+/// Names of the variables that this process set from an implicit `agent.env`
+/// file. A variable that the launcher supplied is never in this set: an
+/// implicit file does not overwrite it.
+pub fn implicit_keys() -> BTreeSet<String> {
+    IMPLICIT_KEYS
+        .lock()
+        .unwrap_or_else(|poisoned| poisoned.into_inner())
+        .clone()
+}
 
 /// Variables that alter loader/interpreter behaviour and are inherited by
 /// exec'd tools — classic injection vectors. Never settable from a file.
@@ -164,6 +184,10 @@ fn apply_content(content: &str, path: &Path, quiet: bool) {
         // the environment yet; mirrors `session::enhance_process_path()`.
         std::env::set_var(key, value);
     }
+    IMPLICIT_KEYS
+        .lock()
+        .unwrap_or_else(|poisoned| poisoned.into_inner())
+        .extend(outcome.set.iter().map(|(key, _)| key.clone()));
     if !quiet && !outcome.set.is_empty() {
         eprintln!(
             "daimonos: loaded {} variable(s) from {}",
diff --git a/src/session_bootstrap.rs b/src/session_bootstrap.rs
index 7040345..86b9cde 100644
--- a/src/session_bootstrap.rs
+++ b/src/session_bootstrap.rs
@@ -72,14 +72,7 @@ pub async fn connect_or_spawn(
 
     let executable = std::env::current_exe()
         .map_err(|error| anyhow::anyhow!("resolve daimonos binary: {error}"))?;
-    let mut command = tokio::process::Command::new(&executable);
-    command
-        .args(daemon_args(options))
-        .env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options))
-        .stdin(Stdio::null())
-        .stdout(Stdio::null())
-        .stderr(Stdio::null())
-        .kill_on_drop(false);
+    let mut command = daemon_command(&executable, options, &crate::env_file::implicit_keys());
     // SAFETY: this closure runs after Command's fork and before exec. setsid,
     // fork, and _exit are async-signal-safe; do not add allocation, logging, or
     // any other non-async-signal-safe operation here. The grandchild returns to
@@ -164,6 +157,32 @@ fn should_bootstrap(error: &std::io::Error) -> bool {
     )
 }
 
+/// The command that starts the session daemon, without its `pre_exec` step.
+///
+/// The daemon does not inherit the variables that this process set from an
+/// implicit `agent.env` file. It has the same workspace and state options, so
+/// it loads the same files again and knows that these values are implicit.
+/// Inherited, they would count as values of the real environment and rank
+/// above the `--agent-env` file (ADR-037).
+fn daemon_command(
+    executable: &Path,
+    options: &BootstrapOptions<'_>,
+    implicit_keys: &std::collections::BTreeSet<String>,
+) -> tokio::process::Command {
+    let mut command = tokio::process::Command::new(executable);
+    command
+        .args(daemon_args(options))
+        .stdin(Stdio::null())
+        .stdout(Stdio::null())
+        .stderr(Stdio::null())
+        .kill_on_drop(false);
+    for key in implicit_keys {
+        command.env_remove(key);
+    }
+    command.env(BOOTSTRAP_FINGERPRINT_ENV, bootstrap_fingerprint(options));
+    command
+}
+
 fn daemon_args(options: &BootstrapOptions<'_>) -> Vec<OsString> {
     let mut args = vec![
         OsString::from("--workspace"),
@@ -323,6 +342,36 @@ fn instance_metadata(socket_path: &Path) -> std::io::Result<Option<serde_json::V
 mod tests {
     use super::*;
 
+    /// ADR-037: the daemon loads the implicit `agent.env` files itself. It
+    /// does not inherit their values as values of the real environment.
+    #[test]
+    fn daemon_command_does_not_pass_on_implicit_agent_env_values() {
+        let directory = tempfile::tempdir().unwrap();
+        let config = directory.path().join("config.toml");
+        let socket = directory.path().join("daemon.sock");
+        let agent_env = directory.path().join("agent.env");
+        let selected = options(directory.path(), &config, &socket, &agent_env);
+        let implicit = ["DAIMONOS_AGENT_MODEL", "DAIMONOS_AGENT_BASE_URL"]
+            .into_iter()
+            .map(String::from)
+            .collect();
+
+        let command = daemon_command(Path::new("/bin/daimonos"), &selected, &implicit);
+        let envs: std::collections::HashMap<_, _> = command
+            .as_std()
+            .get_envs()
+            .map(|(key, value)| (key.to_owned(), value.map(|value| value.to_owned())))
+            .collect();
+        // `None` is a removal from the inherited environment.
+        assert_eq!(envs[std::ffi::OsStr::new("DAIMONOS_AGENT_MODEL")], None);
+        assert_eq!(envs[std::ffi::OsStr::new("DAIMONOS_AGENT_BASE_URL")], None);
+        assert!(envs[std::ffi::OsStr::new(BOOTSTRAP_FINGERPRINT_ENV)].is_some());
+        assert_eq!(envs.len(), 3);
+
+        let none = daemon_command(Path::new("/bin/daimonos"), &selected, &Default::default());
+        assert_eq!(none.as_std().get_envs().count(), 1);
+    }
+
     fn options<'a>(
         workspace: &'a Path,
         config: &'a Path,
diff --git a/tests/test_acp_lifecycle.py b/tests/test_acp_lifecycle.py
index 1637908..6c8e11f 100644
--- a/tests/test_acp_lifecycle.py
+++ b/tests/test_acp_lifecycle.py
@@ -1,37 +1,61 @@
+"""Process lifecycle of `daimonos acp`: stdin EOF and termination signals.
+
+Each process runs with an empty HOME and its own workspace. A developer host
+can have a `~/.config/daimonos/agent.env`, and a test must not read it or send
+requests to the provider that it names (vikunja #1623).
+"""
+import json
 import signal
 import subprocess
-import time
+import threading
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
 
 import pytest
 
+SIGNALS = [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]
 
-def _write_agent_env(tmp_path):
+
+def _spawn_acp(daimonos_binary, tmp_path, base_url="http://127.0.0.1:1"):
+    workspace = tmp_path / "workspace"
+    workspace.mkdir()
+    home = tmp_path / "home"
+    home.mkdir()
     agent_env = tmp_path / "agent.env"
-    agent_env.write_text(
-        "\n".join(
-            [
-                "DAIMONOS_AGENT_PROVIDER=openrouter",
-                "DAIMONOS_AGENT_MODEL=test/model",
-                "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
-                "DAIMONOS_AGENT_APPROVAL_MODE=auto",
-                "DAIMONOS_AGENT_API_KEY=test",
-                "DAIMONOS_AGENT_COMPACTION=off",
-                "",
-            ]
-        )
-    )
-    return agent_env
-
-
-def _spawn_acp(daimonos_binary, tmp_path):
+    agent_env.write_text("\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter",
+        "DAIMONOS_AGENT_MODEL=test/model",
+        f"DAIMONOS_AGENT_BASE_URL={base_url}",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
+        "DAIMONOS_AGENT_API_KEY=test",
+        "DAIMONOS_AGENT_COMPACTION=off",
+        "",
+    ]))
     return subprocess.Popen(
-        [daimonos_binary, "acp", "--agent-env", str(_write_agent_env(tmp_path))],
+        [daimonos_binary, "--workspace", str(workspace), "acp", "--agent-env", str(agent_env)],
+        cwd=workspace,
+        env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
         stdin=subprocess.PIPE,
         stdout=subprocess.PIPE,
         stderr=subprocess.PIPE,
     )
 
 
+def _stop(process):
+    if process.poll() is None:
+        process.kill()
+        process.wait()
+
+
+def _initialize(process):
+    """Complete the ACP handshake. A reply shows that the transport loop runs."""
+    request = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
+               "params": {"protocolVersion": 1, "clientCapabilities": {}}}
+    process.stdin.write((json.dumps(request) + "\n").encode())
+    process.stdin.flush()
+    reply = json.loads(process.stdout.readline())
+    assert reply["id"] == 1 and "result" in reply, reply
+
+
 def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
     process = _spawn_acp(daimonos_binary, tmp_path)
 
@@ -40,12 +64,10 @@ def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
     try:
         assert process.wait(timeout=5) == 0
     finally:
-        if process.poll() is None:
-            process.kill()
-            process.wait()
+        _stop(process)
 
 
-@pytest.mark.parametrize("sig", [signal.SIGTERM, signal.SIGINT, signal.SIGHUP])
+@pytest.mark.parametrize("sig", SIGNALS)
 def test_acp_runs_teardown_when_signaled(daimonos_binary, tmp_path, sig):
     """Signals take the EOF teardown path instead of the default handler (#1544).
 
@@ -54,11 +76,67 @@ def test_acp_runs_teardown_when_signaled(daimonos_binary, tmp_path, sig):
     """
     process = _spawn_acp(daimonos_binary, tmp_path)
     try:
-        time.sleep(1.0)
-        assert process.poll() is None, process.stderr.read().decode()
+        _initialize(process)
         process.send_signal(sig)
         assert process.wait(timeout=10) == 0
     finally:
-        if process.poll() is None:
-            process.kill()
-            process.wait()
+        _stop(process)
+
+
+class HeldCatalog:
+    """A provider endpoint that holds its model-catalog reply until released."""
+
+    def __init__(self):
+        owner = self
+        self.arrived = threading.Event()
+        self.release = threading.Event()
+
+        class Handler(BaseHTTPRequestHandler):
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                owner.arrived.set()
+                owner.release.wait(timeout=30)
+                body = json.dumps({"data": [{"id": "test/model", "context_length": 100000}]}).encode()
+                try:
+                    self.send_response(200)
+                    self.send_header("Content-Type", "application/json")
+                    self.send_header("Content-Length", str(len(body)))
+                    self.end_headers()
+                    self.wfile.write(body)
+                except OSError:
+                    pass  # the client is gone
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        threading.Thread(target=self.server.serve_forever, daemon=True).start()
+        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
+
+    def close(self):
+        self.release.set()
+        self.server.shutdown()
+        self.server.server_close()
+
+
+@pytest.mark.parametrize("sig", SIGNALS)
+def test_acp_exits_cleanly_when_signaled_during_startup(daimonos_binary, tmp_path, sig):
+    """A signal that arrives while startup waits for the provider ends the
+    process with code 0, and at once (#1623).
+
+    Startup asks the provider for its model catalog. Before the fix, the signal
+    handlers were installed after that request, so a signal during the request
+    used the default action: exit by signal, with no teardown.
+    """
+    catalog = HeldCatalog()
+    process = _spawn_acp(daimonos_binary, tmp_path, base_url=catalog.url)
+    try:
+        assert catalog.arrived.wait(timeout=10), "startup did not request the model catalog"
+        process.send_signal(sig)
+        # The catalog reply is still held: the exit does not wait for it. The
+        # limit is below the 5 s that startup waits for the catalog.
+        assert process.wait(timeout=4) == 0
+        assert not catalog.release.is_set()
+    finally:
+        _stop(process)
+        catalog.close()
diff --git a/tests/test_agent_env_precedence.py b/tests/test_agent_env_precedence.py
new file mode 100644
index 0000000..0cbce6e
--- /dev/null
+++ b/tests/test_agent_env_precedence.py
@@ -0,0 +1,353 @@
+"""Precedence of agent settings: real environment > `--agent-env` file > implicit files.
+
+The implicit files are `<workspace>/agent.env` and `~/.config/daimonos/agent.env`.
+Daimonos loads them into the process environment at startup. Before vikunja
+#1623 their values then counted as process values and replaced the values of
+the file that `--agent-env` names.
+
+Each test uses two fake provider endpoints. The endpoint that receives the
+model request shows which file supplied `DAIMONOS_AGENT_BASE_URL`.
+"""
+import json
+import os
+import pty
+import signal
+import subprocess
+import threading
+import time
+from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
+
+from test_agent_oneshot_sessions import Harness, fake_provider, only_json_document, text_reply
+
+__all__ = ["fake_provider"]  # the fixture of the imported module
+
+
+def settings(port, model):
+    return "\n".join([
+        "DAIMONOS_AGENT_PROVIDER=openrouter", f"DAIMONOS_AGENT_MODEL={model}",
+        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{port}", "DAIMONOS_AGENT_API_KEY=test",
+        "DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_COMPACTION=off", ""])
+
+
+def write_home_agent_env(harness, port, model="home/model"):
+    directory = harness.home / ".config" / "daimonos"
+    directory.mkdir(parents=True)
+    (directory / "agent.env").write_text(settings(port, model))
+
+
+def without(harness, name):
+    """Remove one setting from the explicit file of the harness."""
+    lines = [line for line in harness.agent_env.read_text().splitlines() if not line.startswith(name + "=")]
+    harness.agent_env.write_text("\n".join(lines) + "\n")
+
+
+def run(harness, argv, extra_env=None):
+    return subprocess.run(
+        argv, cwd=harness.workspace, env={**harness.environment(), **(extra_env or {})},
+        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
+
+
+def test_explicit_agent_env_wins_over_the_home_agent_env(daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+
+    done = harness.agent("--output", "json", "task")
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE EXPLICIT FILE")
+    assert implicit.requests == [], "the run used the endpoint of the home agent.env"
+    assert len(explicit.requests) == 1
+
+
+def test_explicit_agent_env_wins_over_the_workspace_agent_env_with_a_state_dir(
+        daimonos_binary, tmp_path, fake_provider):
+    # A run with --state-dir does not read the home file. It reads <workspace>/agent.env.
+    implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    harness.config.write_text("[analytics]\nenabled=false\n")
+    (harness.workspace / "agent.env").write_text(settings(implicit.port, "workspace/model"))
+    state = tmp_path / "state"
+    state.mkdir(mode=0o700)
+
+    done = harness.agent("--output", "json", "task", global_args=("--state-dir", str(state)))
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "test/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_a_file_from_the_real_selector_variable_wins_over_the_home_agent_env(
+        daimonos_binary, tmp_path, fake_provider):
+    # DAIMONOS_AGENT_ENV in the real environment selects the file, as --agent-env does.
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE SELECTED FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+
+    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
+            "agent", "--output", "json", "task"]
+    done = run(harness, argv, {"DAIMONOS_AGENT_ENV": str(harness.agent_env)})
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "test/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_a_file_that_an_implicit_file_selects_is_not_an_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    # Unchanged behavior: an implicit file can name the agent env file. That is
+    # not an explicit selection, so a setting of the implicit file still
+    # overrides the file that it names.
+    provider = fake_provider([text_reply("REPLY")])
+    harness = Harness(daimonos_binary, tmp_path, provider.port)
+    (harness.workspace / "agent.env").write_text(
+        f"DAIMONOS_AGENT_ENV={harness.agent_env}\nDAIMONOS_AGENT_MODEL=workspace/model\n")
+
+    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
+            "agent", "--output", "json", "task"]
+    done = run(harness, argv)
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "workspace/model"
+
+
+def test_the_real_environment_wins_over_the_explicit_agent_env(daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+
+    done = run(harness, harness.command("--output", "json", "task"),
+               {"DAIMONOS_AGENT_MODEL": "environment/model"})
+    assert done.returncode == 0, done.stderr
+    # The model comes from the environment. The endpoint comes from the explicit file.
+    assert only_json_document(done.stdout)["model"] == "environment/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_the_home_agent_env_supplies_a_value_that_the_explicit_file_omits(
+        daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+    without(harness, "DAIMONOS_AGENT_MODEL")
+
+    done = harness.agent("--output", "json", "task")
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["model"] == "home/model"
+    assert implicit.requests == []
+    assert len(explicit.requests) == 1
+
+
+def test_the_api_key_of_the_home_agent_env_is_not_sent_to_the_endpoint_of_the_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    # The explicit file names an endpoint and has no API key. The key of the
+    # home file must not go to that endpoint: the run stops before a request.
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+    home_file = harness.home / ".config" / "daimonos" / "agent.env"
+    home_file.write_text(home_file.read_text().replace("API_KEY=test", "API_KEY=sk-home-secret"))
+    without(harness, "DAIMONOS_AGENT_API_KEY")
+
+    done = harness.agent("--output", "json", "task")
+    assert done.returncode != 0
+    assert done.stdout == ""
+    assert "DAIMONOS_AGENT_API_KEY" in done.stderr
+    assert "Not used from an implicit agent.env file" in done.stderr
+    assert "sk-home-secret" not in done.stderr
+    assert implicit.requests == [] and explicit.requests == []
+
+
+def test_the_endpoint_of_the_home_agent_env_does_not_get_the_api_key_of_the_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    # The reverse: the explicit file has a key and no base URL.
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+    without(harness, "DAIMONOS_AGENT_BASE_URL")
+
+    done = harness.agent("--output", "json", "task")
+    assert done.returncode != 0
+    assert "DAIMONOS_AGENT_BASE_URL" in done.stderr
+    assert implicit.requests == [] and explicit.requests == []
+
+
+def test_the_real_environment_can_supply_a_connection_setting_for_the_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port)
+    without(harness, "DAIMONOS_AGENT_API_KEY")
+
+    done = run(harness, harness.command("--output", "json", "task"), {"DAIMONOS_AGENT_API_KEY": "from-the-launcher"})
+    assert done.returncode == 0, done.stderr
+    assert only_json_document(done.stdout)["final_text"] == "FROM THE EXPLICIT FILE"
+    assert implicit.requests == []
+
+
+def test_explicit_agent_env_wins_over_the_workspace_agent_env_under_confinement(
+        daimonos_binary, tmp_path, fake_provider):
+    # With --confine-paths, only <workspace>/agent.env is an implicit file, and
+    # an agent env file must be named. Both ways to name it are explicit.
+    for index, by_option in enumerate([True, False]):
+        implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
+        explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
+        harness = Harness(daimonos_binary, tmp_path / str(index), explicit.port)
+        (harness.workspace / "agent.env").write_text(settings(implicit.port, "workspace/model"))
+        state = tmp_path / str(index) / "state"
+        state.mkdir(mode=0o700)
+        argv = [harness.binary, "--workspace", str(harness.workspace), "--state-dir", str(state),
+                "--confine-paths", "agent", "--output", "json"]
+        extra_env = {}
+        if by_option:
+            argv += ["--agent-env", str(harness.agent_env)]
+        else:
+            extra_env["DAIMONOS_AGENT_ENV"] = str(harness.agent_env)
+        done = run(harness, argv + ["task"], extra_env)
+        assert done.returncode == 0, (by_option, done.stderr)
+        doc = only_json_document(done.stdout)
+        assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE EXPLICIT FILE"), by_option
+        assert implicit.requests == [], by_option
+
+
+def test_the_home_agent_env_is_the_selected_file_without_an_explicit_file(
+        daimonos_binary, tmp_path, fake_provider):
+    # Control: without --agent-env nothing changes. The home file is the selected file.
+    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
+    harness = Harness(daimonos_binary, tmp_path, 1)
+    write_home_agent_env(harness, implicit.port)
+
+    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
+            "agent", "--output", "json", "task"]
+    done = run(harness, argv)
+    assert done.returncode == 0, done.stderr
+    doc = only_json_document(done.stdout)
+    assert (doc["model"], doc["final_text"]) == ("home/model", "FROM THE HOME FILE")
+    assert len(implicit.requests) == 1
+
+
+class CatalogEndpoint:
+    """A provider endpoint that answers the model-catalog request and counts it."""
+
+    def __init__(self):
+        owner = self
+        self.requested = threading.Event()
+
+        class Handler(BaseHTTPRequestHandler):
+            def log_message(self, *_):
+                pass
+
+            def do_GET(self):
+                owner.requested.set()
+                body = json.dumps({"data": [{"id": "test/model", "context_length": 100000}]}).encode()
+                self.send_response(200)
+                self.send_header("Content-Type", "application/json")
+                self.send_header("Content-Length", str(len(body)))
+                self.end_headers()
+                self.wfile.write(body)
+
+        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
+        self.server.daemon_threads = True
+        threading.Thread(target=self.server.serve_forever, daemon=True).start()
+        self.port = self.server.server_address[1]
+
+    def close(self):
+        self.server.shutdown()
+        self.server.server_close()
+
+
+def session_daemon_pids(socket_path):
+    """The session daemon processes that serve `socket_path`, found by command line."""
+    wanted = str(socket_path).encode()
+    pids = []
+    for entry in os.listdir("/proc"):
+        if not entry.isdigit():
+            continue
+        try:
+            with open(f"/proc/{entry}/cmdline", "rb") as handle:
+                argv = handle.read().split(b"\0")
+        except OSError:
+            continue  # the process ended
+        if b"session-daemon" in argv and wanted in argv:
+            pids.append(int(entry))
+    return pids
+
+
+def stop_session_daemons(socket_path, grace=20.0):
+    """Stop each session daemon of `socket_path`, and wait until it is gone.
+
+    The daemon is not a child of the test, and it does not depend on its pid
+    file here: a daemon that is still in its startup has not written one.
+    """
+    for pid in session_daemon_pids(socket_path):
+        try:
+            os.kill(pid, signal.SIGTERM)
+        except ProcessLookupError:
+            pass
+    deadline = time.monotonic() + grace
+    while session_daemon_pids(socket_path) and time.monotonic() < deadline:
+        time.sleep(0.05)
+    for pid in session_daemon_pids(socket_path):
+        try:
+            os.kill(pid, signal.SIGKILL)
+        except ProcessLookupError:
+            pass
+    deadline = time.monotonic() + 5
+    while session_daemon_pids(socket_path) and time.monotonic() < deadline:
+        time.sleep(0.05)
+
+
+def test_the_session_daemon_of_an_interactive_run_uses_the_explicit_agent_env(daimonos_binary, tmp_path):
+    """`agent --interactive` starts the session daemon as a child process. The
+    daemon asks its provider for the model catalog at startup. That request
+    must go to the endpoint of the `--agent-env` file, not to the endpoint of
+    the home `agent.env` that the parent process loaded.
+    """
+    implicit, explicit = CatalogEndpoint(), CatalogEndpoint()
+    try:
+        _daemon_uses_the_explicit_file(daimonos_binary, tmp_path, implicit, explicit)
+    finally:
+        implicit.close()
+        explicit.close()
+
+
+def _daemon_uses_the_explicit_file(daimonos_binary, tmp_path, implicit, explicit):
+    harness = Harness(daimonos_binary, tmp_path, explicit.port)
+    write_home_agent_env(harness, implicit.port, model="test/model")
+    socket_path = tmp_path / "daemon.sock"
+    harness.config.write_text(
+        "[analytics]\nenabled=false\n"
+        f'[session]\nsocket_path = "{socket_path}"\nbootstrap_timeout_secs = 10\n'
+        "bootstrap_retry_interval_ms = 10\n")
+    master_fd, slave_fd = pty.openpty()
+    process = subprocess.Popen(
+        harness.command("--interactive"), cwd=harness.workspace,
+        env={**harness.environment(), "TERM": "xterm"},
+        stdin=slave_fd, stdout=slave_fd, stderr=subprocess.PIPE, text=True)
+    os.close(slave_fd)
+    try:
+        deadline = time.monotonic() + 15
+        while time.monotonic() < deadline and not (explicit.requested.is_set() or implicit.requested.is_set()):
+            assert process.poll() is None, process.stderr.read()
+            time.sleep(0.02)
+        assert not implicit.requested.is_set(), "the daemon used the endpoint of the home agent.env"
+        assert explicit.requested.is_set(), "the daemon requested no model catalog"
+    finally:
+        if process.poll() is None:
+            process.terminate()
+            try:
+                process.wait(timeout=5)
+            except subprocess.TimeoutExpired:
+                process.kill()
+                process.wait()
+        os.close(master_fd)
+        stop_session_daemons(socket_path)
+    assert session_daemon_pids(socket_path) == [], "the test left a session daemon"

### New Points
R1. S1 applied: one predicate agent_env::is_agent_setting (DAIMONOS_AGENT_* or the skill root, and not the selector DAIMONOS_AGENT_ENV) is used at the collection site in AgentEnv::load, in fill_unset_from_implicit, and in merge_process_overrides. The selector is no longer in either map.
R2. S2 applied: the redundant time assertion is removed (the wait timeout of 4 s is the bound, below the 5 s catalog limit of startup). The two HTTP servers of the daemon test are closed in a try/finally that starts before the process is created.
R3. S3 applied: follow-up task Vikunja #1625 for the session daemon and the MCP stdio server, referenced from ADR-037 'Revisit when'.
R4. S4 applied: docs/configuration.md and ADR-037 state that tool child processes inherit the implicit values, that a Daimonos process started by a tool sees them as real values, and that the session daemon does not.
R5. A defect of my own new test, found by the complete pytest run and fixed: the session-daemon test stopped its daemon through the pid file, and a daemon that was still in its startup had no pid file yet, so the test left a daemon and failed on its own cleanup assertion. The test now finds the daemon by its command line in /proc (session-daemon and the socket path), sends SIGTERM, waits, and uses SIGKILL as a last step. After the fix: 3 runs of the three files that start daemons pass, and no session daemon is alive after the complete run.
R6. Scope note: the test test_a_file_that_the_workspace_agent_env_selects_is_not_an_explicit_file of round 1 is replaced by test_a_file_that_an_implicit_file_selects_is_not_an_explicit_file, which shows the same rule with the model setting. The rules for a run WITHOUT an explicitly selected file, including the order of the two implicit files, are outside ADR-037 and are tracked in Vikunja #1624.
R7. Verification of b0238d3 on the worker host: fmt and clippy exit 0. Rust suite with an empty HOME: 2150 passed, 6 failed (plugins::shellcheck::tests::*, no shellcheck on the host), 10 ignored. pytest tests: 611 passed, 7 failed, 1 skipped; all 7 failures are tests/test_shellcheck.py (counted from the pytest FAILED lines, by file). tests/test_agent_env_precedence.py has 12 tests and tests/test_acp_lifecycle.py has 7 cases; both files pass in 2 runs. CI on b0238d3 was running at the time of this rebuttal.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re B1: The ACCEPT is complete and the fix is the stronger of the two options I offered: rather than documenting an exception, the code now enforces the decision driver. is_connection_setting covers PROVIDER, every *_BASE_URL, and every *_API_KEY (including secondary providers, which my concern had not even named), fill_unset_from_implicit withholds them, and the startup error names the withheld settings without leaking values. The revised diff shows the Rust predicate/behavior tests and the two directional process tests (home key not sent to explicit endpoint, explicit key not sent to home endpoint) plus the real-environment supply path; the mutation check (connection check disabled fails both process tests) demonstrates the tests detect the exact regression. ADR-037 Decision and Consequences now state the rule and its migration cost. Resolved.
Re B2: The CLARIFY resolves both parts. (a) The residual window is now measured (12-18 ms post-fix vs up to the full provider delay pre-fix), documented in the install-site comment, and the CHANGELOG no longer claims 'installed first' but 'before those requests', which is accurate. I accept the argument that moving install into main buys only milliseconds at the cost of mode-specific signal state in main, and that a pre-runtime signal can never be caught; the window is inherent and now honestly bounded and documented. (b) My premise about analytics ordering was wrong: prepare_acp opens the store after the provider waits, so a signal during the catalog request finds no open store. The drop-during-open case is also accounted for: spawn_blocking runs to completion, the store drops and closes the connection, and WAL-mode SQLite with synchronous=NORMAL keeps the file consistent; no analytics record exists yet so no flush is needed. The comment at the select now states this. The install-failure nonzero exit is unchanged in kind from the old `?` on the inline registrations, which I accept. Resolved.
Re B3: The CLARIFY resolves the concern. The path-by-path account shows all four load paths funnel through AgentEnv::load, which keeps its public signature (only the private seam changed), so load_explicit compiles unchanged and inherits the new explicit/implicit split; the confined path passes Some(selected) so flag.is_some() makes it explicit, and caller_agent_env is captured in main before any implicit file loads, so it is genuinely real-environment. My worry that the state-dir test could pass with explicit=false is directly refuted by the mutation run: forcing explicit=false fails 6 of 8 process tests including that one, because the workspace file sets a conflicting endpoint and model. The new confinement test covers the one path that previously had no process-level coverage, with both selection mechanisms. The timing point is answered: implicit_keys and resolve_path are both read inside load after env_file loading completes, with no intervening writes. Resolved.
Re R1: Confirmed in the revised diff: is_agent_setting is the single predicate used at the collection site, in merge_process_overrides, and in fill_unset_from_implicit, and DAIMONOS_AGENT_ENV is excluded at collection so neither map carries the selector. The asymmetry I flagged is gone. S1 is satisfied.
Re R2: Confirmed: the redundant monotonic assertion is removed and the comment explains why the 4 s wait bound is meaningful (below the 5 s catalog timeout). The daemon test's endpoints are now created inside a try/finally that encloses the Popen, so a spawn failure no longer leaks the servers. S2 is satisfied.
Re R3: Confirmed: Vikunja #1625 is filed for the session daemon and MCP stdio server signal-handler ordering and is referenced from ADR-037's Revisit-when section. S3 is satisfied; the follow-up is now tracked outside this review.
Re R4: Confirmed: docs/configuration.md and ADR-037 Consequences now state that tool children inherit implicit values, that a Daimonos process a tool starts sees them as real, and that the session daemon is the exception. S4 is satisfied.
Re R5: Good catch and a sound fix. The pid-file dependency was a real cleanup hole for a daemon still in startup; discovering the daemon by /proc cmdline (session-daemon plus the socket path) with SIGTERM, a bounded wait, and SIGKILL fallback is robust, and the final assertion now verifies no daemon remains by the same mechanism rather than by socket existence alone. The /proc scan is Linux-specific, which matches the unix-only scope of these tests. No further action needed.
Re R6: The scope note is reasonable. The replacement test still proves the rule (an implicit-file selector is not explicit) via the model setting, which is actually clearer now that connection settings are withheld under explicit selection — the old base-URL variant would have conflated two rules. Deferring the implicit-file ordering question to Vikunja #1624, recorded in ADR-037 Open questions, is an appropriate boundary for this decision.
Re R7: The verification picture is consistent: the only Rust and pytest failures are the known shellcheck-host gaps, the new test counts (12 precedence, 7 lifecycle) match the diff, and the mutation checks for B1 and B3 were run on this revision. Please confirm CI on b0238d3 is green before merge, since it was still running at rebuttal time — this is a merge-gate note, not a blocking concern, as the local evidence is thorough.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
