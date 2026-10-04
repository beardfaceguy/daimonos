# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="vikunja-1198-evidence-review" -->

<!-- event id="request" artifact path="vikunja-1198-evidence-review-vikunja-1198/artifacts/round-1-review-request.diff" sha256="f1e5f6572fed249f6c81b2015ec5a6142b8968e5cd1f5fb7a1f0b679d3191701" -->
## Review Request — Round 1
**Task:** vikunja-1198-evidence-review — Observed-evidence completion signal for autonomous agent turns
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Fingerprint each workspace before and after tool calls using an external shadow git repository; record mutations and passing verifier-shaped exec calls in ordinal order; expose Verified, Unverified, NoMutations, or Unknown on AgentResult/TurnResult; warn in print and ACP frontends without blocking.

### Relevant Code / Diff
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index ddc71ac..d3b3954 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -1949,6 +1949,28 @@ async fn run_prompt_turn(
 
     Ok(match execution.outcome {
         SessionPromptOutcome::Completed(turn) => {
+            match turn.evidence.status() {
+                crate::evidence::EvidenceStatus::Unverified => send_notification(
+                    cx,
+                    session_id,
+                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
+                        TextContent::new(
+                            "Warning: the workspace changed after the last successful verifier; completion is unverified.",
+                        ),
+                    ))),
+                ),
+                crate::evidence::EvidenceStatus::Unknown => send_notification(
+                    cx,
+                    session_id,
+                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
+                        TextContent::new(
+                            "Warning: workspace changes could not be observed; completion evidence is unknown.",
+                        ),
+                    ))),
+                ),
+                crate::evidence::EvidenceStatus::NoMutations
+                | crate::evidence::EvidenceStatus::Verified => {}
+            }
             emit_usage_update(
                 cx,
                 session_id,
@@ -7756,6 +7778,7 @@ mod tests {
             stop_reason,
             error_message: None,
             context_overflow: false,
+            evidence: Default::default(),
         };
         assert_eq!(
             canonical_assistant_outcome(&turn(crate::providers::StopReason::EndTurn)),
diff --git i/src/agent.rs w/src/agent.rs
index 98e5f42..277f04d 100644
--- i/src/agent.rs
+++ w/src/agent.rs
@@ -307,6 +307,9 @@ pub struct AgentResult {
     /// The final call failed as a classified context-window overflow; the
     /// reactive compaction path keys off this to compact and retry once.
     pub context_overflow: bool,
+    /// What this turn was observed to do: whether the workspace changed and
+    /// whether a verifier passed afterwards (vikunja #1198). Report-only.
+    pub evidence: crate::evidence::EvidenceLedger,
 }
 
 // --- Pure helpers ---
@@ -942,6 +945,18 @@ async fn run_inner(
             (None, 0)
         }
     };
+    // #1198: observed-evidence ledger. Mutations are detected by comparing
+    // worktree fingerprints around each tool call rather than by classifying
+    // tool names, so `exec`-mediated edits and reverts are caught too. The
+    // baseline is taken lazily before the first tool call, so a turn that never
+    // calls a tool pays nothing.
+    let evidence_tree = {
+        let guard = session.lock().await;
+        crate::evidence::Worktree::new(guard.workspace.clone(), &crate::paths::state_dir())
+    };
+    let mut evidence = crate::evidence::EvidenceLedger::default();
+    let mut evidence_fingerprint: Option<String> = None;
+    let mut evidence_degraded = false;
 
     loop {
         // Deterministically shed old successful tool context before every
@@ -1268,6 +1283,7 @@ async fn run_inner(
                     error_message: resp.error_message,
                     last_call_usage: resp.usage,
                     context_overflow: resp.context_overflow,
+                    evidence,
                 };
             }
             StopReason::ToolUse => {
@@ -1291,6 +1307,18 @@ async fn run_inner(
                 let mut round_observations = Vec::new();
                 let mut terminate = false;
 
+                // Baseline for this turn's first tool call. Taken here so
+                // read-only turns never shell out to git at all.
+                if evidence_fingerprint.is_none() && !evidence_degraded {
+                    match evidence_tree.fingerprint().await {
+                        Some(fingerprint) => evidence_fingerprint = Some(fingerprint),
+                        None => {
+                            evidence_degraded = true;
+                            evidence.mark_fingerprint_unavailable();
+                        }
+                    }
+                }
+
                 for (id, name, input) in calls {
                     let info = ToolCallInfo {
                         id: id.clone(),
@@ -1612,6 +1640,25 @@ async fn run_inner(
                             }
                         }
                     };
+                    // #1198: record what this call was observed to do, using the
+                    // raw result — the verifier's exit status must be read
+                    // before output bounding can truncate it away.
+                    let verifier = crate::evidence::verifier_exit(&name, &input, &content);
+                    if !evidence_degraded {
+                        match evidence_tree.fingerprint().await {
+                            Some(after) => {
+                                let changed =
+                                    evidence_fingerprint.as_deref().is_some_and(|b| b != after);
+                                evidence.record_call(changed, verifier);
+                                evidence_fingerprint = Some(after);
+                            }
+                            None => {
+                                evidence_degraded = true;
+                                evidence.mark_fingerprint_unavailable();
+                            }
+                        }
+                    }
+
                     // after_tool_call hook
                     if let Some(hook) = &config.after_tool_call {
                         if matches!(hook(&info, &content, is_error), AfterHookResult::Terminate) {
@@ -1803,6 +1850,7 @@ async fn run_inner(
                         error_message: Some("terminated by after_tool_call hook".to_string()),
                         last_call_usage: resp.usage,
                         context_overflow: false,
+                        evidence,
                     };
                 }
 
@@ -1844,6 +1892,7 @@ async fn run_inner(
                                 error_message: Some(message),
                                 last_call_usage: resp.usage,
                                 context_overflow: false,
+                                evidence,
                             };
                         }
                     }
@@ -1867,6 +1916,8 @@ pub struct TurnResult {
     pub stop_reason: StopReason,
     pub error_message: Option<String>,
     pub context_overflow: bool,
+    /// Observed evidence for this turn (vikunja #1198).
+    pub evidence: crate::evidence::EvidenceLedger,
 }
 
 /// A stateful, re-promptable agent conversation wrapping the one-shot [`run`]
@@ -2132,6 +2183,7 @@ impl AgentSession {
             stop_reason: result.stop_reason,
             error_message: result.error_message,
             context_overflow: result.context_overflow,
+            evidence: result.evidence,
         }
     }
 
@@ -6518,4 +6570,180 @@ mod tests {
             )
         }));
     }
+
+    /// A stub verifier named so `exec_filter::classify` sees a test runner:
+    /// it strips the directory, so `<dir>/pytest` classifies as `TestRunner`
+    /// without needing pytest installed.
+    fn stub_verifier(dir: &std::path::Path, name: &str, exit_code: i32) -> String {
+        use std::os::unix::fs::PermissionsExt;
+        let path = dir.join(name);
+        std::fs::write(&path, format!("#!/bin/sh\nexit {exit_code}\n")).unwrap();
+        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
+        path.to_str().unwrap().to_string()
+    }
+
+    #[tokio::test]
+    async fn evidence_reports_verified_when_mutation_precedes_passing_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let verifier = stub_verifier(dir.path(), "pytest", 0);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            tool_call_resp("v1", "exec", json!({"command": verifier})),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it and prove it").await;
+
+        assert_eq!(turn.stop_reason, StopReason::EndTurn);
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Verified,
+            "a mutation followed by a passing verifier is verified"
+        );
+    }
+
+    /// The shape both `sphinx-9229` and `django-12273` produced in the
+    /// 2026-09-11 full-50 run: the workspace changed and the turn ended with a
+    /// confident summary, but nothing verified the change.
+    #[tokio::test]
+    async fn evidence_reports_unverified_when_mutation_has_no_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            end_turn_resp_with_text("All relevant test suites pass."),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a claim of passing tests must not be backed by an absent verifier"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_reports_unverified_when_mutation_follows_passing_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let verifier = stub_verifier(dir.path(), "pytest", 0);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp("v1", "exec", json!({"command": verifier})),
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "late.txt", "content": "edited after the green run"}),
+            ),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("test then edit").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a verifier cannot vouch for an edit made after it ran"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_does_not_count_a_failing_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let verifier = stub_verifier(dir.path(), "pytest", 1);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            tool_call_resp("v1", "exec", json!({"command": verifier})),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a red verifier is not evidence"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_reports_no_mutations_for_a_read_only_turn() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp("r1", "read_file", json!({"path": "f.txt"})),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("what does f.txt say?").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::NoMutations
+        );
+    }
+
+    /// The case a mutating-tool allowlist cannot see, and the reason #1239
+    /// rejected one: the edit arrives through `exec`, not through `write_file`.
+    #[tokio::test]
+    async fn evidence_detects_a_mutation_made_through_exec() {
+        let dir = tempfile::tempdir().unwrap();
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "exec",
+                json!({"command": "echo patched > via_exec.txt"}),
+            ),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("patch it with a shell redirect").await;
+
+        assert!(
+            dir.path().join("via_exec.txt").exists(),
+            "precondition: the shell redirect must have landed"
+        );
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "an exec-mediated edit is still a mutation"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_does_not_treat_an_install_as_a_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let installer = stub_verifier(dir.path(), "pip", 0);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            tool_call_resp(
+                "i1",
+                "exec",
+                json!({"command": format!("{installer} install x")}),
+            ),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a successful install proves nothing about the code"
+        );
+    }
 }
diff --git i/src/agent_cmd.rs w/src/agent_cmd.rs
index 6d1d231..2f48ec3 100644
--- i/src/agent_cmd.rs
+++ w/src/agent_cmd.rs
@@ -136,6 +136,7 @@ pub async fn run_agent(
             error_message: Some("dry-run".to_string()),
             last_call_usage: Default::default(),
             context_overflow: false,
+            evidence: Default::default(),
         });
     }
 
diff --git i/src/agent_runtime.rs w/src/agent_runtime.rs
index 1a3fe5a..ac19d05 100644
--- i/src/agent_runtime.rs
+++ w/src/agent_runtime.rs
@@ -1046,6 +1046,16 @@ fn require_agent_task(
 }
 
 fn check_agent_result(result: &agent::AgentResult) -> anyhow::Result<()> {
+    match result.evidence.status() {
+        crate::evidence::EvidenceStatus::Unverified => eprintln!(
+            "warning: the workspace changed after the last successful verifier; completion is unverified"
+        ),
+        crate::evidence::EvidenceStatus::Unknown => eprintln!(
+            "warning: workspace changes could not be observed; completion evidence is unknown"
+        ),
+        crate::evidence::EvidenceStatus::NoMutations
+        | crate::evidence::EvidenceStatus::Verified => {}
+    }
     if result.stop_reason == providers::StopReason::Error {
         let message = result.error_message.as_deref().unwrap_or("unknown error");
         anyhow::bail!("agent error: {message}");
@@ -1259,6 +1269,7 @@ mod tests {
             error_message: Some("provider unavailable".to_string()),
             last_call_usage: Default::default(),
             context_overflow: false,
+            evidence: Default::default(),
         };
 
         let error = check_agent_result(&result).unwrap_err();
diff --git i/src/paths.rs w/src/paths.rs
index 7c34f58..53732e8 100644
--- i/src/paths.rs
+++ w/src/paths.rs
@@ -24,6 +24,20 @@ pub(crate) fn expand_tilde(path: &str) -> PathBuf {
     }
 }
 
+/// Global Daimonos state directory. Uses `~/.daimonos`, with a per-user temp
+/// fallback when HOME is unavailable so evidence stores are never put inside
+/// the workspace they fingerprint.
+pub(crate) fn state_dir() -> PathBuf {
+    if let Some(home) = home_dir() {
+        return home.join(".daimonos");
+    }
+    let user = std::env::var("UID")
+        .ok()
+        .or_else(|| std::env::var("USER").ok())
+        .unwrap_or_else(|| "unknown".to_string());
+    std::env::temp_dir().join(format!("daimonos-{user}"))
+}
+
 /// Canonical durable store shared by ACP and daemon-owned agent sessions.
 pub(crate) fn agent_sessions_dir() -> Option<PathBuf> {
     home_dir().map(|home| home.join(".daimonos").join("acp-sessions"))diff --git 1/src/evidence.rs 2/src/evidence.rs
new file mode 100644
index 0000000..08ecbc2
--- /dev/null
+++ 2/src/evidence.rs
@@ -0,0 +1,339 @@
+//! Observed-evidence ledger for autonomous coding turns (vikunja #1198).
+//!
+//! A turn that edits the workspace and then claims "tests pass" is only
+//! trustworthy if a verifier actually ran *after* the edit. This module records
+//! what was observed so a frontend can say so without a second model call.
+//!
+//! Mutations are detected by fingerprinting the worktree, never by classifying
+//! tool names. `agent.rs` already rejected a mutating-tool allowlist for
+//! checkpoints (#1239, after the enumeration drift of #1113): `exec` and
+//! `execute_script` can mutate anything, and a `git checkout` that reverts the
+//! model's own work is a mutation no name list would flag. The fingerprint is a
+//! git tree object built in a shadow repository, so it is content-addressed,
+//! honours the workspace's `.gitignore`, and never touches the user's index.
+//!
+//! When the fingerprint is unavailable the status is [`EvidenceStatus::Unknown`]
+//! rather than "no mutations" — reporting unobserved absence as evidence is the
+//! exact failure this ledger exists to prevent.
+
+use std::path::{Path, PathBuf};
+use std::process::Stdio;
+
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub enum EvidenceStatus {
+    /// The worktree could not be fingerprinted, so mutations were not
+    /// observable. Distinct from `NoMutations`, which is a real observation.
+    Unknown,
+    /// No tool call changed the worktree during the turn.
+    NoMutations,
+    /// A verifier exited zero after the last observed mutation.
+    Verified,
+    /// The worktree changed and no verifier has passed since.
+    Unverified,
+}
+
+/// Ordered record of mutations and verifier outcomes within one turn.
+///
+/// Positions are call ordinals, so "verified" means a passing verifier was
+/// observed strictly after the newest mutation. A single call that both mutates
+/// and verifies therefore does not count: the tree it reported on is not the
+/// tree it left behind.
+#[derive(Debug, Default, Clone)]
+pub struct EvidenceLedger {
+    calls: u64,
+    last_mutation: Option<u64>,
+    last_passing_verifier: Option<u64>,
+    fingerprint_unavailable: bool,
+}
+
+impl EvidenceLedger {
+    /// Record one completed tool call.
+    pub fn record_call(&mut self, tree_changed: bool, verifier_exit: Option<i64>) {
+        self.calls += 1;
+        if tree_changed {
+            self.last_mutation = Some(self.calls);
+        }
+        if verifier_exit == Some(0) {
+            self.last_passing_verifier = Some(self.calls);
+        }
+    }
+
+    /// Note that fingerprinting failed, so later observations are incomplete.
+    pub fn mark_fingerprint_unavailable(&mut self) {
+        self.fingerprint_unavailable = true;
+    }
+
+    pub fn status(&self) -> EvidenceStatus {
+        match (self.last_mutation, self.last_passing_verifier) {
+            // A mutation was seen, so the signal is usable even if a later
+            // fingerprint failed: the tree is known to have changed.
+            (Some(mutation), Some(verifier)) if verifier > mutation => EvidenceStatus::Verified,
+            (Some(_), _) => EvidenceStatus::Unverified,
+            // Nothing observed. Only claim "no mutations" when we could look.
+            (None, _) if self.fingerprint_unavailable => EvidenceStatus::Unknown,
+            (None, _) => EvidenceStatus::NoMutations,
+        }
+    }
+}
+
+/// The exit status of a tool call that verifies the workspace, or `None` when
+/// the call is not a verifier.
+///
+/// Verifier shape is decided by [`crate::ops::exec_filter::classify`], the same
+/// classifier that decides output filtering, so the two cannot drift. Installs
+/// are excluded: a successful `pip install` proves nothing about the code.
+pub fn verifier_exit(tool_name: &str, input: &serde_json::Value, content: &str) -> Option<i64> {
+    if tool_name != "exec" {
+        return None;
+    }
+    let command = input.get("command").and_then(serde_json::Value::as_str)?;
+    use crate::ops::exec_filter::ExecFilter;
+    match crate::ops::exec_filter::classify(command) {
+        ExecFilter::TestRunner | ExecFilter::Build | ExecFilter::Linter => {}
+        ExecFilter::Install | ExecFilter::None => return None,
+    }
+    serde_json::from_str::<serde_json::Value>(content)
+        .ok()?
+        .get("exit")
+        .and_then(serde_json::Value::as_i64)
+}
+
+/// Per-workspace shadow git dir under the global daimonos state dir:
+/// `<base>/evidence/<workspace-key>.git`, keyed by a stable hash of the
+/// canonicalized workspace path — the same scheme ADR-009 uses for
+/// coordination databases.
+///
+/// It deliberately lives **outside** the workspace. An in-tree shadow repo
+/// shows up as untracked files in the user's own `git status`, which both
+/// pollutes the tree and changes what the agent observes — corrupting the very
+/// thing this module measures.
+pub fn workspace_git_dir(base: &Path, workspace: &Path) -> PathBuf {
+    use sha2::{Digest, Sha256};
+    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
+    let mut hasher = Sha256::new();
+    hasher.update(canonical.to_string_lossy().as_bytes());
+    let key: String = hasher
+        .finalize()
+        .iter()
+        .map(|b| format!("{b:02x}"))
+        .collect();
+    base.join("evidence").join(format!("{key}.git"))
+}
+
+/// Content fingerprint of a workspace, computed in a private shadow repository.
+pub struct Worktree {
+    workspace: PathBuf,
+    git_dir: PathBuf,
+}
+
+impl Worktree {
+    /// Build a fingerprinter for `workspace`, storing its shadow repo under
+    /// `state_base` (normally `~/.daimonos`).
+    pub fn new(workspace: impl Into<PathBuf>, state_base: &Path) -> Self {
+        let workspace = workspace.into();
+        let git_dir = workspace_git_dir(state_base, &workspace);
+        Self { workspace, git_dir }
+    }
+
+    /// Content hash of the current worktree, or `None` when it cannot be
+    /// computed (no git binary, unreadable workspace). Never an error: absent
+    /// evidence is reported as absent, not as a failed turn.
+    pub async fn fingerprint(&self) -> Option<String> {
+        self.init().await.ok()?;
+        // `add -A` refreshes the index; `write-tree` hashes it. No commit, so
+        // the shadow repo stays a single reusable index rather than a history.
+        self.git(&["add", "-A"]).await.ok()?;
+        let tree = self.git(&["write-tree"]).await.ok()?;
+        Some(tree.trim().to_string())
+    }
+
+    async fn init(&self) -> Result<(), String> {
+        if self.git_dir.join("HEAD").exists() {
+            return Ok(());
+        }
+        if let Some(parent) = self.git_dir.parent() {
+            tokio::fs::create_dir_all(parent)
+                .await
+                .map_err(|e| format!("create {}: {e}", parent.display()))?;
+        }
+        let out = tokio::process::Command::new("git")
+            .args(["init", "--bare", "--quiet"])
+            .arg(&self.git_dir)
+            .stdin(Stdio::null())
+            .output()
+            .await
+            .map_err(|e| format!("git init: {e}"))?;
+        if !out.status.success() {
+            return Err(format!(
+                "git init failed: {}",
+                String::from_utf8_lossy(&out.stderr).trim()
+            ));
+        }
+        // Exclude our own store and the checkpoint store, or the fingerprint
+        // would change every time either of them writes.
+        let info = self.git_dir.join("info");
+        tokio::fs::create_dir_all(&info)
+            .await
+            .map_err(|e| format!("create info: {e}"))?;
+        tokio::fs::write(info.join("exclude"), ".daimonos/\n")
+            .await
+            .map_err(|e| format!("write exclude: {e}"))?;
+        Ok(())
+    }
+
+    /// Run one git command against the shadow repo. `--git-dir`/`--work-tree`
+    /// are always explicit and the ambient git environment is stripped, so a
+    /// stray `GIT_DIR` cannot redirect this into the real repository.
+    async fn git(&self, args: &[&str]) -> Result<String, String> {
+        let out = tokio::process::Command::new("git")
+            .arg("--git-dir")
+            .arg(&self.git_dir)
+            .arg("--work-tree")
+            .arg(&self.workspace)
+            .args(args)
+            .current_dir(&self.workspace)
+            .env_remove("GIT_DIR")
+            .env_remove("GIT_WORK_TREE")
+            .env_remove("GIT_INDEX_FILE")
+            .stdin(Stdio::null())
+            .output()
+            .await
+            .map_err(|e| format!("git {}: {e}", args.first().unwrap_or(&"")))?;
+        if !out.status.success() {
+            return Err(format!(
+                "git {} failed: {}",
+                args.first().unwrap_or(&""),
+                String::from_utf8_lossy(&out.stderr).trim()
+            ));
+        }
+        Ok(String::from_utf8_lossy(&out.stdout).to_string())
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use serde_json::json;
+
+    #[test]
+    fn a_verifier_that_dirties_the_tree_it_reports_on_does_not_verify() {
+        let mut ledger = EvidenceLedger::default();
+        // One call that both mutated and exited zero: the tree it measured is
+        // not the tree it left behind.
+        ledger.record_call(true, Some(0));
+        assert_eq!(ledger.status(), EvidenceStatus::Unverified);
+    }
+
+    #[test]
+    fn an_unobservable_worktree_is_unknown_rather_than_clean() {
+        let mut ledger = EvidenceLedger::default();
+        ledger.mark_fingerprint_unavailable();
+        assert_eq!(
+            ledger.status(),
+            EvidenceStatus::Unknown,
+            "never report absence of mutations we could not look for"
+        );
+    }
+
+    #[test]
+    fn an_observed_mutation_survives_a_later_fingerprint_failure() {
+        let mut ledger = EvidenceLedger::default();
+        ledger.record_call(true, None);
+        ledger.mark_fingerprint_unavailable();
+        assert_eq!(
+            ledger.status(),
+            EvidenceStatus::Unverified,
+            "the tree is known to have changed, so the signal is still usable"
+        );
+    }
+
+    #[test]
+    fn repeated_verifiers_keep_the_newest_position() {
+        let mut ledger = EvidenceLedger::default();
+        ledger.record_call(false, Some(0));
+        ledger.record_call(true, None);
+        ledger.record_call(false, Some(0));
+        assert_eq!(ledger.status(), EvidenceStatus::Verified);
+    }
+
+    #[test]
+    fn verifier_exit_reads_test_runner_status() {
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "pytest -q"}), r#"{"exit":0}"#),
+            Some(0)
+        );
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "cargo test"}), r#"{"exit":101}"#),
+            Some(101)
+        );
+    }
+
+    #[test]
+    fn verifier_exit_rejects_non_verifier_shapes() {
+        // Not a verifier command.
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "ls -la"}), r#"{"exit":0}"#),
+            None
+        );
+        // Installs are excluded on purpose.
+        assert_eq!(
+            verifier_exit(
+                "exec",
+                &json!({"command": "pip install x"}),
+                r#"{"exit":0}"#
+            ),
+            None
+        );
+        // Not the exec tool.
+        assert_eq!(
+            verifier_exit("read_file", &json!({"path": "pytest"}), r#"{"exit":0}"#),
+            None
+        );
+        // Unparseable or exit-less result.
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "pytest"}), "not json"),
+            None
+        );
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "pytest"}), r#"{"out":"hi"}"#),
+            None
+        );
+    }
+
+    #[tokio::test]
+    async fn fingerprint_changes_only_when_content_changes() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
+
+        let first = tree.fingerprint().await.expect("git available");
+        let unchanged = tree.fingerprint().await.unwrap();
+        assert_eq!(first, unchanged, "an untouched tree hashes identically");
+
+        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
+        let changed = tree.fingerprint().await.unwrap();
+        assert_ne!(first, changed, "edited content must change the hash");
+
+        // Re-editing an already-dirty file must still register. A status-based
+        // fingerprint would report " M a.txt" both times and miss this.
+        std::fs::write(dir.path().join("a.txt"), "three").unwrap();
+        assert_ne!(changed, tree.fingerprint().await.unwrap());
+
+        // Reverting returns to the original hash: the tree really is unchanged.
+        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
+        assert_eq!(first, tree.fingerprint().await.unwrap());
+    }
+
+    #[tokio::test]
+    async fn fingerprint_works_in_a_workspace_that_is_not_a_repository() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("loose.txt"), "no repo here").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
+        assert!(
+            tree.fingerprint().await.is_some(),
+            "the shadow repo is ours, so the workspace needs no git of its own"
+        );
+    }
+}

### Known Concerns
1. Full cargo test has 1856 passes but 13 environment-sensitive failures because npm and shellcheck are absent and one existing inotify-cap test exceeds its configured count. cargo check, all-target/all-feature clippy -D warnings, and 12 evidence tests pass.
2. The MCP commit already on HEAD declares mod evidence; this scoped patch adds the module and integration.

### Specific Questions for Reviewer
1. Does the fingerprinting correctly represent mutation order and dirty worktrees?
2. Is verifier classification too narrow or unsafe?
3. Are external state paths, failure degradation, and user warnings correct?
4. Does this satisfy Vikunja #1198 acceptance without becoming a blocking gate?

<!-- event id="1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Concurrent-session index corruption: the shadow repo is keyed only by canonicalized workspace path, and fingerprint() runs `git add -A` + `write-tree` against a single shared index with no locking. Two concurrent turns (or two daimonos processes) on the same workspace will race on the same index file, producing spurious 'changed' fingerprints, git lock failures (index.lock contention), or silently wrong evidence. At minimum key the shadow index per session/turn (e.g. GIT_INDEX_FILE per turn) or take an advisory lock; otherwise Verified/Unverified can be actively wrong rather than merely absent.
B2. Fingerprint honors the workspace's .gitignore, so a mutation to an ignored-but-relevant file (e.g. a generated config the agent edits, or .env) is invisible, but worse: the shadow repo's info/exclude only excludes `.daimonos/`, while the workspace's own `.git/` directory is NOT excluded. `git add -A` from a shadow repo over a workspace containing a real `.git` directory will skip it (git refuses to add nested .git), but a `git checkout`/`git commit` performed by the agent mutates only `.git/` plus tracked files — a revert via `git checkout` changes worktree content and is caught, but a `git commit` (mutating only .git) after edits is not, which is fine; the real gap is ignored-path mutations. Please document or handle: mutations confined to gitignored paths are reported as NoMutations, which is a false 'real observation' per the module's own stated invariant. Either add-with-`--force` semantics scoped sensibly, or downgrade such turns' confidence in docs and the vikunja acceptance discussion.
B3. Unbounded growth and cross-run staleness of the shadow index: the shadow repo persists under ~/.daimonos/evidence/<hash>.git forever, accumulating loose objects on every fingerprint (`git add -A` writes blobs for all changed files; large workspaces write large object stores each turn). There is no gc, no size cap, and no cleanup for deleted workspaces. Also, because the index persists across runs, the FIRST fingerprint of a new turn is computed relative to a stale index — that's fine for the baseline (it's absolute content hashing via write-tree), but the object store growth is real. Add periodic `git gc`/prune, use `GIT_OBJECT_DIRECTORY` in a temp dir, or use `git hash-object`-free plumbing (e.g. `git add` with a throwaway objects dir) — or at least an ADR-acknowledged growth bound.
B4. verifier_exit only recognizes the `exec` tool and requires the raw content to be JSON with a top-level `exit` integer. If `execute_script` (mentioned in the module docs as a mutation vector) or any other exec-like tool runs the test suite, the passing verifier is ignored and the turn is reported Unverified — a false negative warning that will train users to ignore the signal. Either cover execute_script (and any tool whose result carries an exit status) or document why exec-only is acceptable for #1198 acceptance.

### Non-Blocking Suggestions
S1. Failure-mode asymmetry in the loop: once evidence_degraded is set, verifier passes are also no longer recorded (record_call is only invoked inside the fingerprint-success arm). A turn that mutated early, then lost fingerprinting, then ran a green verifier reports Unverified even though the verifier outcome was observable independently of the fingerprint. Consider recording verifier_exit unconditionally and only gating the tree_changed bit on fingerprint availability.
S2. Fingerprint cost: two full `git add -A` passes per tool call (before is amortized to once, after is per-call) on large workspaces adds noticeable latency to every tool call in the hot loop. Consider fingerprinting only after tool calls that could plausibly mutate (still content-based, but skip after pure read_file), or batching per model round rather than per call — order within a round is already available from call ordinals.
S3. state_dir() UID/USER fallback: `UID` is a shell variable, not typically exported to the environment, so the fallback will usually resolve via USER or 'unknown'; a shared temp dir named daimonos-unknown is a multi-user collision/symlink risk. Prefer libc getuid() or at least restrict permissions on the created dir.
S4. The known 13 environment-sensitive cargo test failures should be explicitly confirmed as pre-existing on HEAD (not introduced or worsened by this patch), ideally with a one-line note comparing failure sets before/after; the inotify-cap count exceedance in particular could plausibly be aggravated by the shadow-repo file churn.
S5. ACP warning is emitted as AgentThoughtChunk, which many frontends render de-emphasized or collapse; a user-visible advisory (e.g. an agent message chunk or a dedicated notice type) better matches 'warn without blocking'. Non-blocking, but worth aligning with how print mode uses eprintln.

**Verdict:** REVISE

<!-- event id="2" artifact path="vikunja-1198-evidence-review-vikunja-1198/artifacts/round-1-rebuttal.diff" sha256="9524212e3c86569c87deeb217809f3c5fc641bcb9a3f5159a3adac620501db80" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — Each Worktree now gets a UUID-named per-turn bare repository under the workspace hash, so concurrent sessions/processes never share an index, lock, or object store. A concurrent fingerprint test verifies independent dirs and identical results.
Re B2: ACCEPT — Fingerprinting now uses git add --force with explicit exclusions for .git and .daimonos, so ignored workspace files are observed. Added a test that changing an ignored file changes the fingerprint.
Re B3: ACCEPT — Per-turn shadow repositories are removed on Worktree drop, bounding object growth to one live turn. Added cleanup test.
Re B4: CLARIFY — I disagree that aggregate execute_script success can safely count as verifier success. Its result does not preserve which nested command produced which exit status; scripts can run a failing test and still return successfully, or run no verifier. Treating the wrapper result as green would fabricate evidence. The docs now state this explicitly. Nested ops need structured per-op outcomes in a future change before they can prove verification; current conservative behavior is a warning-only false negative, not a false positive.

### Revised Code / Diff
diff --git i/src/acp_cmd.rs w/src/acp_cmd.rs
index ddc71ac..d3b3954 100644
--- i/src/acp_cmd.rs
+++ w/src/acp_cmd.rs
@@ -1949,6 +1949,28 @@ async fn run_prompt_turn(
 
     Ok(match execution.outcome {
         SessionPromptOutcome::Completed(turn) => {
+            match turn.evidence.status() {
+                crate::evidence::EvidenceStatus::Unverified => send_notification(
+                    cx,
+                    session_id,
+                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
+                        TextContent::new(
+                            "Warning: the workspace changed after the last successful verifier; completion is unverified.",
+                        ),
+                    ))),
+                ),
+                crate::evidence::EvidenceStatus::Unknown => send_notification(
+                    cx,
+                    session_id,
+                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
+                        TextContent::new(
+                            "Warning: workspace changes could not be observed; completion evidence is unknown.",
+                        ),
+                    ))),
+                ),
+                crate::evidence::EvidenceStatus::NoMutations
+                | crate::evidence::EvidenceStatus::Verified => {}
+            }
             emit_usage_update(
                 cx,
                 session_id,
@@ -7756,6 +7778,7 @@ mod tests {
             stop_reason,
             error_message: None,
             context_overflow: false,
+            evidence: Default::default(),
         };
         assert_eq!(
             canonical_assistant_outcome(&turn(crate::providers::StopReason::EndTurn)),
diff --git i/src/agent.rs w/src/agent.rs
index 98e5f42..b3a1e27 100644
--- i/src/agent.rs
+++ w/src/agent.rs
@@ -307,6 +307,9 @@ pub struct AgentResult {
     /// The final call failed as a classified context-window overflow; the
     /// reactive compaction path keys off this to compact and retry once.
     pub context_overflow: bool,
+    /// What this turn was observed to do: whether the workspace changed and
+    /// whether a verifier passed afterwards (vikunja #1198). Report-only.
+    pub evidence: crate::evidence::EvidenceLedger,
 }
 
 // --- Pure helpers ---
@@ -942,6 +945,18 @@ async fn run_inner(
             (None, 0)
         }
     };
+    // #1198: observed-evidence ledger. Mutations are detected by comparing
+    // worktree fingerprints around each tool call rather than by classifying
+    // tool names, so `exec`-mediated edits and reverts are caught too. The
+    // baseline is taken lazily before the first tool call, so a turn that never
+    // calls a tool pays nothing.
+    let evidence_tree = {
+        let guard = session.lock().await;
+        crate::evidence::Worktree::new(guard.workspace.clone(), &crate::paths::state_dir())
+    };
+    let mut evidence = crate::evidence::EvidenceLedger::default();
+    let mut evidence_fingerprint: Option<String> = None;
+    let mut evidence_degraded = false;
 
     loop {
         // Deterministically shed old successful tool context before every
@@ -1268,6 +1283,7 @@ async fn run_inner(
                     error_message: resp.error_message,
                     last_call_usage: resp.usage,
                     context_overflow: resp.context_overflow,
+                    evidence,
                 };
             }
             StopReason::ToolUse => {
@@ -1291,6 +1307,18 @@ async fn run_inner(
                 let mut round_observations = Vec::new();
                 let mut terminate = false;
 
+                // Baseline for this turn's first tool call. Taken here so
+                // read-only turns never shell out to git at all.
+                if evidence_fingerprint.is_none() && !evidence_degraded {
+                    match evidence_tree.fingerprint().await {
+                        Some(fingerprint) => evidence_fingerprint = Some(fingerprint),
+                        None => {
+                            evidence_degraded = true;
+                            evidence.mark_fingerprint_unavailable();
+                        }
+                    }
+                }
+
                 for (id, name, input) in calls {
                     let info = ToolCallInfo {
                         id: id.clone(),
@@ -1612,6 +1640,30 @@ async fn run_inner(
                             }
                         }
                     };
+                    // #1198: record what this call was observed to do, using the
+                    // raw result — the verifier's exit status must be read
+                    // before output bounding can truncate it away.
+                    let verifier = crate::evidence::verifier_exit(&name, &input, &content);
+                    if !evidence_degraded {
+                        match evidence_tree.fingerprint().await {
+                            Some(after) => {
+                                let changed =
+                                    evidence_fingerprint.as_deref().is_some_and(|b| b != after);
+                                evidence.record_call(changed, verifier);
+                                evidence_fingerprint = Some(after);
+                            }
+                            None => {
+                                evidence_degraded = true;
+                                evidence.mark_fingerprint_unavailable();
+                                evidence.record_call(false, verifier);
+                            }
+                        }
+                    } else {
+                        // Verifier outcome remains observable even if later
+                        // mutation fingerprinting has degraded.
+                        evidence.record_call(false, verifier);
+                    }
+
                     // after_tool_call hook
                     if let Some(hook) = &config.after_tool_call {
                         if matches!(hook(&info, &content, is_error), AfterHookResult::Terminate) {
@@ -1803,6 +1855,7 @@ async fn run_inner(
                         error_message: Some("terminated by after_tool_call hook".to_string()),
                         last_call_usage: resp.usage,
                         context_overflow: false,
+                        evidence,
                     };
                 }
 
@@ -1844,6 +1897,7 @@ async fn run_inner(
                                 error_message: Some(message),
                                 last_call_usage: resp.usage,
                                 context_overflow: false,
+                                evidence,
                             };
                         }
                     }
@@ -1867,6 +1921,8 @@ pub struct TurnResult {
     pub stop_reason: StopReason,
     pub error_message: Option<String>,
     pub context_overflow: bool,
+    /// Observed evidence for this turn (vikunja #1198).
+    pub evidence: crate::evidence::EvidenceLedger,
 }
 
 /// A stateful, re-promptable agent conversation wrapping the one-shot [`run`]
@@ -2132,6 +2188,7 @@ impl AgentSession {
             stop_reason: result.stop_reason,
             error_message: result.error_message,
             context_overflow: result.context_overflow,
+            evidence: result.evidence,
         }
     }
 
@@ -6518,4 +6575,180 @@ mod tests {
             )
         }));
     }
+
+    /// A stub verifier named so `exec_filter::classify` sees a test runner:
+    /// it strips the directory, so `<dir>/pytest` classifies as `TestRunner`
+    /// without needing pytest installed.
+    fn stub_verifier(dir: &std::path::Path, name: &str, exit_code: i32) -> String {
+        use std::os::unix::fs::PermissionsExt;
+        let path = dir.join(name);
+        std::fs::write(&path, format!("#!/bin/sh\nexit {exit_code}\n")).unwrap();
+        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
+        path.to_str().unwrap().to_string()
+    }
+
+    #[tokio::test]
+    async fn evidence_reports_verified_when_mutation_precedes_passing_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let verifier = stub_verifier(dir.path(), "pytest", 0);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            tool_call_resp("v1", "exec", json!({"command": verifier})),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it and prove it").await;
+
+        assert_eq!(turn.stop_reason, StopReason::EndTurn);
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Verified,
+            "a mutation followed by a passing verifier is verified"
+        );
+    }
+
+    /// The shape both `sphinx-9229` and `django-12273` produced in the
+    /// 2026-09-11 full-50 run: the workspace changed and the turn ended with a
+    /// confident summary, but nothing verified the change.
+    #[tokio::test]
+    async fn evidence_reports_unverified_when_mutation_has_no_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            end_turn_resp_with_text("All relevant test suites pass."),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a claim of passing tests must not be backed by an absent verifier"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_reports_unverified_when_mutation_follows_passing_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let verifier = stub_verifier(dir.path(), "pytest", 0);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp("v1", "exec", json!({"command": verifier})),
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "late.txt", "content": "edited after the green run"}),
+            ),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("test then edit").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a verifier cannot vouch for an edit made after it ran"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_does_not_count_a_failing_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let verifier = stub_verifier(dir.path(), "pytest", 1);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            tool_call_resp("v1", "exec", json!({"command": verifier})),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a red verifier is not evidence"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_reports_no_mutations_for_a_read_only_turn() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp("r1", "read_file", json!({"path": "f.txt"})),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("what does f.txt say?").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::NoMutations
+        );
+    }
+
+    /// The case a mutating-tool allowlist cannot see, and the reason #1239
+    /// rejected one: the edit arrives through `exec`, not through `write_file`.
+    #[tokio::test]
+    async fn evidence_detects_a_mutation_made_through_exec() {
+        let dir = tempfile::tempdir().unwrap();
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "exec",
+                json!({"command": "echo patched > via_exec.txt"}),
+            ),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("patch it with a shell redirect").await;
+
+        assert!(
+            dir.path().join("via_exec.txt").exists(),
+            "precondition: the shell redirect must have landed"
+        );
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "an exec-mediated edit is still a mutation"
+        );
+    }
+
+    #[tokio::test]
+    async fn evidence_does_not_treat_an_install_as_a_verifier() {
+        let dir = tempfile::tempdir().unwrap();
+        let installer = stub_verifier(dir.path(), "pip", 0);
+        let provider = Box::new(MockProvider::new(vec![
+            tool_call_resp(
+                "m1",
+                "write_file",
+                json!({"path": "fix.txt", "content": "fixed"}),
+            ),
+            tool_call_resp(
+                "i1",
+                "exec",
+                json!({"command": format!("{installer} install x")}),
+            ),
+            end_turn_resp(),
+        ]));
+        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
+        let turn = sess.prompt("fix it").await;
+
+        assert_eq!(
+            turn.evidence.status(),
+            crate::evidence::EvidenceStatus::Unverified,
+            "a successful install proves nothing about the code"
+        );
+    }
 }
diff --git i/src/agent_cmd.rs w/src/agent_cmd.rs
index 6d1d231..2f48ec3 100644
--- i/src/agent_cmd.rs
+++ w/src/agent_cmd.rs
@@ -136,6 +136,7 @@ pub async fn run_agent(
             error_message: Some("dry-run".to_string()),
             last_call_usage: Default::default(),
             context_overflow: false,
+            evidence: Default::default(),
         });
     }
 
diff --git i/src/agent_runtime.rs w/src/agent_runtime.rs
index 1a3fe5a..ac19d05 100644
--- i/src/agent_runtime.rs
+++ w/src/agent_runtime.rs
@@ -1046,6 +1046,16 @@ fn require_agent_task(
 }
 
 fn check_agent_result(result: &agent::AgentResult) -> anyhow::Result<()> {
+    match result.evidence.status() {
+        crate::evidence::EvidenceStatus::Unverified => eprintln!(
+            "warning: the workspace changed after the last successful verifier; completion is unverified"
+        ),
+        crate::evidence::EvidenceStatus::Unknown => eprintln!(
+            "warning: workspace changes could not be observed; completion evidence is unknown"
+        ),
+        crate::evidence::EvidenceStatus::NoMutations
+        | crate::evidence::EvidenceStatus::Verified => {}
+    }
     if result.stop_reason == providers::StopReason::Error {
         let message = result.error_message.as_deref().unwrap_or("unknown error");
         anyhow::bail!("agent error: {message}");
@@ -1259,6 +1269,7 @@ mod tests {
             error_message: Some("provider unavailable".to_string()),
             last_call_usage: Default::default(),
             context_overflow: false,
+            evidence: Default::default(),
         };
 
         let error = check_agent_result(&result).unwrap_err();
diff --git i/src/paths.rs w/src/paths.rs
index 7c34f58..104923b 100644
--- i/src/paths.rs
+++ w/src/paths.rs
@@ -24,6 +24,22 @@ pub(crate) fn expand_tilde(path: &str) -> PathBuf {
     }
 }
 
+/// Global Daimonos state directory. Uses `~/.daimonos`, with a per-user temp
+/// fallback when HOME is unavailable so evidence stores are never put inside
+/// the workspace they fingerprint.
+pub(crate) fn state_dir() -> PathBuf {
+    if let Some(home) = home_dir() {
+        return home.join(".daimonos");
+    }
+    let user = std::env::var("UID")
+        .ok()
+        .or_else(|| std::env::var("USER").ok())
+        .unwrap_or_else(|| "unknown".to_string());
+    // The per-process suffix avoids sharing a predictable directory when the
+    // platform exposes neither UID nor USER. Evidence state is ephemeral.
+    std::env::temp_dir().join(format!("daimonos-{user}-{}", std::process::id()))
+}
+
 /// Canonical durable store shared by ACP and daemon-owned agent sessions.
 pub(crate) fn agent_sessions_dir() -> Option<PathBuf> {
     home_dir().map(|home| home.join(".daimonos").join("acp-sessions"))diff --git 1/src/evidence.rs 2/src/evidence.rs
new file mode 100644
index 0000000..bb62a27
--- /dev/null
+++ 2/src/evidence.rs
@@ -0,0 +1,413 @@
+//! Observed-evidence ledger for autonomous coding turns (vikunja #1198).
+//!
+//! A turn that edits the workspace and then claims "tests pass" is only
+//! trustworthy if a verifier actually ran *after* the edit. This module records
+//! what was observed so a frontend can say so without a second model call.
+//!
+//! Mutations are detected by fingerprinting the worktree, never by classifying
+//! tool names. `agent.rs` already rejected a mutating-tool allowlist for
+//! checkpoints (#1239, after the enumeration drift of #1113): `exec` and
+//! `execute_script` can mutate anything, and a `git checkout` that reverts the
+//! model's own work is a mutation no name list would flag. The fingerprint is a
+//! git tree object built in a per-turn shadow repository, so it is
+//! content-addressed, includes ignored workspace content, never touches the
+//! user's index, and is removed at the end of the turn.
+//!
+//! When the fingerprint is unavailable the status is [`EvidenceStatus::Unknown`]
+//! rather than "no mutations" — reporting unobserved absence as evidence is the
+//! exact failure this ledger exists to prevent.
+
+use std::path::{Path, PathBuf};
+use std::process::Stdio;
+
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub enum EvidenceStatus {
+    /// The worktree could not be fingerprinted, so mutations were not
+    /// observable. Distinct from `NoMutations`, which is a real observation.
+    Unknown,
+    /// No tool call changed the worktree during the turn.
+    NoMutations,
+    /// A verifier exited zero after the last observed mutation.
+    Verified,
+    /// The worktree changed and no verifier has passed since.
+    Unverified,
+}
+
+/// Ordered record of mutations and verifier outcomes within one turn.
+///
+/// Positions are call ordinals, so "verified" means a passing verifier was
+/// observed strictly after the newest mutation. A single call that both mutates
+/// and verifies therefore does not count: the tree it reported on is not the
+/// tree it left behind.
+#[derive(Debug, Default, Clone)]
+pub struct EvidenceLedger {
+    calls: u64,
+    last_mutation: Option<u64>,
+    last_passing_verifier: Option<u64>,
+    fingerprint_unavailable: bool,
+}
+
+impl EvidenceLedger {
+    /// Record one completed tool call.
+    pub fn record_call(&mut self, tree_changed: bool, verifier_exit: Option<i64>) {
+        self.calls += 1;
+        if tree_changed {
+            self.last_mutation = Some(self.calls);
+        }
+        if verifier_exit == Some(0) {
+            self.last_passing_verifier = Some(self.calls);
+        }
+    }
+
+    /// Note that fingerprinting failed, so later observations are incomplete.
+    pub fn mark_fingerprint_unavailable(&mut self) {
+        self.fingerprint_unavailable = true;
+    }
+
+    pub fn status(&self) -> EvidenceStatus {
+        match (self.last_mutation, self.last_passing_verifier) {
+            // A mutation was seen, so the signal is usable even if a later
+            // fingerprint failed: the tree is known to have changed.
+            (Some(mutation), Some(verifier)) if verifier > mutation => EvidenceStatus::Verified,
+            (Some(_), _) => EvidenceStatus::Unverified,
+            // Nothing observed. Only claim "no mutations" when we could look.
+            (None, _) if self.fingerprint_unavailable => EvidenceStatus::Unknown,
+            (None, _) => EvidenceStatus::NoMutations,
+        }
+    }
+}
+
+/// The exit status of a tool call that verifies the workspace, or `None` when
+/// the call is not a verifier.
+///
+/// Verifier shape is decided by [`crate::ops::exec_filter::classify`], the same
+/// classifier that decides output filtering, so the two cannot drift. Installs
+/// are excluded: a successful `pip install` proves nothing about the code.
+/// `execute_script` is intentionally not inferred from arbitrary source: its
+/// aggregate result does not preserve which nested command produced which exit
+/// status, so treating script success as test success would fabricate evidence.
+pub fn verifier_exit(tool_name: &str, input: &serde_json::Value, content: &str) -> Option<i64> {
+    if tool_name != "exec" {
+        return None;
+    }
+    let command = input.get("command").and_then(serde_json::Value::as_str)?;
+    use crate::ops::exec_filter::ExecFilter;
+    match crate::ops::exec_filter::classify(command) {
+        ExecFilter::TestRunner | ExecFilter::Build | ExecFilter::Linter => {}
+        ExecFilter::Install | ExecFilter::None => return None,
+    }
+    serde_json::from_str::<serde_json::Value>(content)
+        .ok()?
+        .get("exit")
+        .and_then(serde_json::Value::as_i64)
+}
+
+/// Per-turn shadow git dir under the global daimonos state dir:
+/// `<base>/evidence/<workspace-key>/<instance>.git`. The workspace key groups
+/// diagnostics; the random instance prevents concurrent sessions from sharing
+/// an index, lock, or object store.
+///
+/// It deliberately lives **outside** the workspace. An in-tree shadow repo
+/// shows up as untracked files in the user's own `git status`, which both
+/// pollutes the tree and changes what the agent observes — corrupting the very
+/// thing this module measures.
+pub fn workspace_git_dir(base: &Path, workspace: &Path) -> PathBuf {
+    use sha2::{Digest, Sha256};
+    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
+    let mut hasher = Sha256::new();
+    hasher.update(canonical.to_string_lossy().as_bytes());
+    let key: String = hasher
+        .finalize()
+        .iter()
+        .map(|b| format!("{b:02x}"))
+        .collect();
+    base.join("evidence").join(key)
+}
+
+/// Content fingerprint of a workspace, computed in a private shadow repository.
+pub struct Worktree {
+    workspace: PathBuf,
+    git_dir: PathBuf,
+}
+
+impl Worktree {
+    /// Build a fingerprinter for `workspace`, storing its shadow repo under
+    /// `state_base` (normally `~/.daimonos`).
+    pub fn new(workspace: impl Into<PathBuf>, state_base: &Path) -> Self {
+        let workspace = workspace.into();
+        let git_dir =
+            workspace_git_dir(state_base, &workspace).join(format!("{}.git", uuid::Uuid::new_v4()));
+        Self { workspace, git_dir }
+    }
+
+    /// Content hash of the current worktree, or `None` when it cannot be
+    /// computed (no git binary, unreadable workspace). Never an error: absent
+    /// evidence is reported as absent, not as a failed turn.
+    pub async fn fingerprint(&self) -> Option<String> {
+        self.init().await.ok()?;
+        // Force ignored content into the observation: an agent can mutate a
+        // generated or secret-bearing ignored file, and calling that turn clean
+        // would be false evidence. Only repository/state internals are excluded.
+        self.git(&[
+            "add",
+            "--force",
+            "-A",
+            "--",
+            ".",
+            ":(exclude).git",
+            ":(exclude).git/**",
+            ":(exclude).daimonos",
+            ":(exclude).daimonos/**",
+        ])
+        .await
+        .ok()?;
+        let tree = self.git(&["write-tree"]).await.ok()?;
+        Some(tree.trim().to_string())
+    }
+
+    async fn init(&self) -> Result<(), String> {
+        if self.git_dir.join("HEAD").exists() {
+            return Ok(());
+        }
+        if let Some(parent) = self.git_dir.parent() {
+            tokio::fs::create_dir_all(parent)
+                .await
+                .map_err(|e| format!("create {}: {e}", parent.display()))?;
+        }
+        let out = tokio::process::Command::new("git")
+            .args(["init", "--bare", "--quiet"])
+            .arg(&self.git_dir)
+            .stdin(Stdio::null())
+            .output()
+            .await
+            .map_err(|e| format!("git init: {e}"))?;
+        if !out.status.success() {
+            return Err(format!(
+                "git init failed: {}",
+                String::from_utf8_lossy(&out.stderr).trim()
+            ));
+        }
+        // Exclude our own store and the checkpoint store, or the fingerprint
+        // would change every time either of them writes. The explicit add
+        // pathspecs above retain these exclusions even with --force.
+        let info = self.git_dir.join("info");
+        tokio::fs::create_dir_all(&info)
+            .await
+            .map_err(|e| format!("create info: {e}"))?;
+        tokio::fs::write(info.join("exclude"), ".daimonos/\n")
+            .await
+            .map_err(|e| format!("write exclude: {e}"))?;
+        Ok(())
+    }
+
+    /// Run one git command against the shadow repo. `--git-dir`/`--work-tree`
+    /// are always explicit and the ambient git environment is stripped, so a
+    /// stray `GIT_DIR` cannot redirect this into the real repository.
+    async fn git(&self, args: &[&str]) -> Result<String, String> {
+        let out = tokio::process::Command::new("git")
+            .arg("--git-dir")
+            .arg(&self.git_dir)
+            .arg("--work-tree")
+            .arg(&self.workspace)
+            .args(args)
+            .current_dir(&self.workspace)
+            .env_remove("GIT_DIR")
+            .env_remove("GIT_WORK_TREE")
+            .env_remove("GIT_INDEX_FILE")
+            .stdin(Stdio::null())
+            .output()
+            .await
+            .map_err(|e| format!("git {}: {e}", args.first().unwrap_or(&"")))?;
+        if !out.status.success() {
+            return Err(format!(
+                "git {} failed: {}",
+                args.first().unwrap_or(&""),
+                String::from_utf8_lossy(&out.stderr).trim()
+            ));
+        }
+        Ok(String::from_utf8_lossy(&out.stdout).to_string())
+    }
+}
+
+impl Drop for Worktree {
+    fn drop(&mut self) {
+        // Per-turn object stores are bounded by turn lifetime. Best effort:
+        // shutdown must never fail merely because cleanup could not complete.
+        let _ = std::fs::remove_dir_all(&self.git_dir);
+        if let Some(workspace_dir) = self.git_dir.parent() {
+            let _ = std::fs::remove_dir(workspace_dir);
+        }
+    }
+}
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use serde_json::json;
+
+    #[test]
+    fn a_verifier_that_dirties_the_tree_it_reports_on_does_not_verify() {
+        let mut ledger = EvidenceLedger::default();
+        // One call that both mutated and exited zero: the tree it measured is
+        // not the tree it left behind.
+        ledger.record_call(true, Some(0));
+        assert_eq!(ledger.status(), EvidenceStatus::Unverified);
+    }
+
+    #[test]
+    fn an_unobservable_worktree_is_unknown_rather_than_clean() {
+        let mut ledger = EvidenceLedger::default();
+        ledger.mark_fingerprint_unavailable();
+        assert_eq!(
+            ledger.status(),
+            EvidenceStatus::Unknown,
+            "never report absence of mutations we could not look for"
+        );
+    }
+
+    #[test]
+    fn an_observed_mutation_survives_a_later_fingerprint_failure() {
+        let mut ledger = EvidenceLedger::default();
+        ledger.record_call(true, None);
+        ledger.mark_fingerprint_unavailable();
+        assert_eq!(
+            ledger.status(),
+            EvidenceStatus::Unverified,
+            "the tree is known to have changed, so the signal is still usable"
+        );
+    }
+
+    #[test]
+    fn repeated_verifiers_keep_the_newest_position() {
+        let mut ledger = EvidenceLedger::default();
+        ledger.record_call(false, Some(0));
+        ledger.record_call(true, None);
+        ledger.record_call(false, Some(0));
+        assert_eq!(ledger.status(), EvidenceStatus::Verified);
+    }
+
+    #[test]
+    fn verifier_exit_reads_test_runner_status() {
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "pytest -q"}), r#"{"exit":0}"#),
+            Some(0)
+        );
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "cargo test"}), r#"{"exit":101}"#),
+            Some(101)
+        );
+    }
+
+    #[test]
+    fn verifier_exit_rejects_non_verifier_shapes() {
+        // Not a verifier command.
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "ls -la"}), r#"{"exit":0}"#),
+            None
+        );
+        // Installs are excluded on purpose.
+        assert_eq!(
+            verifier_exit(
+                "exec",
+                &json!({"command": "pip install x"}),
+                r#"{"exit":0}"#
+            ),
+            None
+        );
+        // Not the exec tool.
+        assert_eq!(
+            verifier_exit("read_file", &json!({"path": "pytest"}), r#"{"exit":0}"#),
+            None
+        );
+        // Unparseable or exit-less result.
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "pytest"}), "not json"),
+            None
+        );
+        assert_eq!(
+            verifier_exit("exec", &json!({"command": "pytest"}), r#"{"out":"hi"}"#),
+            None
+        );
+    }
+
+    #[tokio::test]
+    async fn fingerprint_changes_only_when_content_changes() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
+
+        let first = tree.fingerprint().await.expect("git available");
+        let unchanged = tree.fingerprint().await.unwrap();
+        assert_eq!(first, unchanged, "an untouched tree hashes identically");
+
+        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
+        let changed = tree.fingerprint().await.unwrap();
+        assert_ne!(first, changed, "edited content must change the hash");
+
+        // Re-editing an already-dirty file must still register. A status-based
+        // fingerprint would report " M a.txt" both times and miss this.
+        std::fs::write(dir.path().join("a.txt"), "three").unwrap();
+        assert_ne!(changed, tree.fingerprint().await.unwrap());
+
+        // Reverting returns to the original hash: the tree really is unchanged.
+        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
+        assert_eq!(first, tree.fingerprint().await.unwrap());
+    }
+
+    #[tokio::test]
+    async fn fingerprint_includes_gitignored_content() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(
+            dir.path().join(".gitignore"),
+            "ignored.txt
+",
+        )
+        .unwrap();
+        std::fs::write(dir.path().join("ignored.txt"), "one").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
+        let first = tree.fingerprint().await.unwrap();
+        std::fs::write(dir.path().join("ignored.txt"), "two").unwrap();
+        assert_ne!(first, tree.fingerprint().await.unwrap());
+    }
+
+    #[tokio::test]
+    async fn concurrent_fingerprinters_use_independent_shadow_repositories() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let left = Worktree::new(dir.path().to_path_buf(), state.path());
+        let right = Worktree::new(dir.path().to_path_buf(), state.path());
+        assert_ne!(left.git_dir, right.git_dir);
+        let (a, b) = tokio::join!(left.fingerprint(), right.fingerprint());
+        assert_eq!(a, b);
+    }
+
+    #[tokio::test]
+    async fn shadow_repository_is_removed_on_drop() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let path;
+        {
+            let tree = Worktree::new(dir.path().to_path_buf(), state.path());
+            path = tree.git_dir.clone();
+            assert!(tree.fingerprint().await.is_some());
+            assert!(path.exists());
+        }
+        assert!(!path.exists());
+    }
+
+    #[tokio::test]
+    async fn fingerprint_works_in_a_workspace_that_is_not_a_repository() {
+        let dir = tempfile::tempdir().unwrap();
+        std::fs::write(dir.path().join("loose.txt"), "no repo here").unwrap();
+        let state = tempfile::tempdir().unwrap();
+        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
+        assert!(
+            tree.fingerprint().await.is_some(),
+            "the shadow repo is ours, so the workspace needs no git of its own"
+        );
+    }
+}

### New Points
R1. Accepted S1: verifier outcomes are recorded even after fingerprint degradation. Accepted S3: HOME-less state paths include process id to avoid a predictable cross-process directory. S2 is deferred because skipping tools by name violates the ticket's content-observation invariant. S5 remains thought-channel by design because this is advisory telemetry.
R2. Validation: cargo check and all-target/all-feature clippy -D warnings pass; evidence module now has 11/11 passing tests and the 4 agent acceptance tests pass.

**Requesting:** RE-REVIEW
