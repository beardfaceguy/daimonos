//! Observed-evidence ledger for autonomous coding turns (vikunja #1198).
//!
//! A turn that edits the workspace and then claims "tests pass" is only
//! trustworthy if a verifier actually ran *after* the edit. This module records
//! what was observed so a frontend can say so without a second model call.
//!
//! Mutations are detected by fingerprinting the worktree, never by classifying
//! tool names. `agent.rs` already rejected a mutating-tool allowlist for
//! checkpoints (#1239, after the enumeration drift of #1113): `exec` and
//! `execute_script` can mutate anything, and a `git checkout` that reverts the
//! model's own work is a mutation no name list would flag. The fingerprint is a
//! git tree object built in a per-turn shadow repository, so it is
//! content-addressed, includes ignored workspace content, never touches the
//! user's index, and is removed at the end of the turn.
//!
//! When the fingerprint is unavailable the status is [`EvidenceStatus::Unknown`]
//! rather than "no mutations" — reporting unobserved absence as evidence is the
//! exact failure this ledger exists to prevent.

use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceStatus {
    /// The worktree could not be fingerprinted, so mutations were not
    /// observable. Distinct from `NoMutations`, which is a real observation.
    Unknown,
    /// No tool call changed the worktree during the turn.
    NoMutations,
    /// A verifier exited zero after the last observed mutation.
    Verified,
    /// The worktree changed and no verifier has passed since.
    Unverified,
}

/// Ordered record of mutations and verifier outcomes within one turn.
///
/// Positions are call ordinals, so "verified" means a passing verifier was
/// observed strictly after the newest mutation. A single call that both mutates
/// and verifies therefore does not count: the tree it reported on is not the
/// tree it left behind.
#[derive(Debug, Default, Clone)]
pub struct EvidenceLedger {
    calls: u64,
    last_mutation: Option<u64>,
    last_passing_verifier: Option<u64>,
    fingerprint_unavailable: bool,
}

impl EvidenceLedger {
    /// Record one completed tool call.
    pub fn record_call(&mut self, tree_changed: bool, verifier_exit: Option<i64>) {
        self.calls += 1;
        if tree_changed {
            self.last_mutation = Some(self.calls);
        }
        if verifier_exit == Some(0) {
            self.last_passing_verifier = Some(self.calls);
        }
    }

    /// Note that fingerprinting failed, so later observations are incomplete.
    pub fn mark_fingerprint_unavailable(&mut self) {
        self.fingerprint_unavailable = true;
    }

    pub fn status(&self) -> EvidenceStatus {
        match (self.last_mutation, self.last_passing_verifier) {
            // A mutation was seen, so the signal is usable even if a later
            // fingerprint failed: the tree is known to have changed.
            (Some(mutation), Some(verifier)) if verifier > mutation => EvidenceStatus::Verified,
            (Some(_), _) => EvidenceStatus::Unverified,
            // Nothing observed. Only claim "no mutations" when we could look.
            (None, _) if self.fingerprint_unavailable => EvidenceStatus::Unknown,
            (None, _) => EvidenceStatus::NoMutations,
        }
    }
}

/// The exit status of a tool call that verifies the workspace, or `None` when
/// the call is not a verifier.
///
/// Verifier shape is decided by [`crate::ops::exec_filter::classify`], the same
/// classifier that decides output filtering, so the two cannot drift. Installs
/// are excluded: a successful `pip install` proves nothing about the code.
/// Compound shell commands are excluded because the top-level exit status may
/// belong to a later command (`pytest; true`) or pipeline sink (`pytest | tee`)
/// rather than to the verifier itself.
/// `execute_script` is intentionally not inferred from arbitrary source: its
/// aggregate result does not preserve which nested command produced which exit
/// status, so treating script success as test success would fabricate evidence.
pub fn verifier_exit(tool_name: &str, input: &serde_json::Value, content: &str) -> Option<i64> {
    if tool_name != "exec" {
        return None;
    }
    let command = input.get("command").and_then(serde_json::Value::as_str)?;
    if command
        .chars()
        .any(|c| matches!(c, ';' | '\n' | '\r' | '|' | '&' | '`'))
        || command.contains("$(")
    {
        return None;
    }
    use crate::ops::exec_filter::ExecFilter;
    match crate::ops::exec_filter::classify(command) {
        ExecFilter::TestRunner | ExecFilter::Build | ExecFilter::Linter => {}
        ExecFilter::Install | ExecFilter::None => return None,
    }
    serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .get("exit")
        .and_then(serde_json::Value::as_i64)
}

/// Per-turn shadow git dir under the global daimonos state dir:
/// `<base>/evidence/<workspace-key>/<instance>.git`. The workspace key groups
/// diagnostics; the random instance prevents concurrent sessions from sharing
/// an index, lock, or object store.
///
/// It deliberately lives **outside** the workspace. An in-tree shadow repo
/// shows up as untracked files in the user's own `git status`, which both
/// pollutes the tree and changes what the agent observes — corrupting the very
/// thing this module measures.
pub fn workspace_git_dir(base: &Path, workspace: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let key: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    base.join("evidence").join(key)
}

/// Content fingerprint of a workspace, computed in a private shadow repository.
pub struct Worktree {
    workspace: PathBuf,
    git_dir: PathBuf,
}

impl Worktree {
    /// Build a fingerprinter for `workspace`, storing its shadow repo under
    /// `state_base` (normally `~/.daimonos`).
    pub fn new(workspace: impl Into<PathBuf>, state_base: &Path) -> Self {
        let workspace = workspace.into();
        let git_dir =
            workspace_git_dir(state_base, &workspace).join(format!("{}.git", uuid::Uuid::new_v4()));
        Self { workspace, git_dir }
    }

    /// Content hash of the current worktree, or `None` when it cannot be
    /// computed (no git binary, unreadable workspace). Never an error: absent
    /// evidence is reported as absent, not as a failed turn.
    pub async fn fingerprint(&self) -> Option<String> {
        self.init().await.ok()?;
        // Force ignored content into the observation: an agent can mutate a
        // generated or secret-bearing ignored file, and calling that turn clean
        // would be false evidence. Only repository/state internals are excluded.
        self.git(&[
            "add",
            "--force",
            "-A",
            "--",
            ".",
            ":(exclude).git",
            ":(exclude).git/**",
            ":(exclude).daimonos",
            ":(exclude).daimonos/**",
        ])
        .await
        .ok()?;
        let tree = self.git(&["write-tree"]).await.ok()?;
        Some(tree.trim().to_string())
    }

    async fn init(&self) -> Result<(), String> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        if let Some(parent) = self.git_dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let out = tokio::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&self.git_dir)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("git init: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // Exclude our own store and the checkpoint store, or the fingerprint
        // would change every time either of them writes. The explicit add
        // pathspecs above retain these exclusions even with --force.
        let info = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info)
            .await
            .map_err(|e| format!("create info: {e}"))?;
        tokio::fs::write(info.join("exclude"), ".daimonos/\n")
            .await
            .map_err(|e| format!("write exclude: {e}"))?;
        Ok(())
    }

    /// Run one git command against the shadow repo. `--git-dir`/`--work-tree`
    /// are always explicit and the ambient git environment is stripped, so a
    /// stray `GIT_DIR` cannot redirect this into the real repository.
    async fn git(&self, args: &[&str]) -> Result<String, String> {
        let out = tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.workspace)
            .args(args)
            .current_dir(&self.workspace)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("git {}: {e}", args.first().unwrap_or(&"")))?;
        if !out.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Per-turn object stores are bounded by turn lifetime. Best effort:
        // shutdown must never fail merely because cleanup could not complete.
        let _ = std::fs::remove_dir_all(&self.git_dir);
        if let Some(workspace_dir) = self.git_dir.parent() {
            let _ = std::fs::remove_dir(workspace_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_verifier_that_dirties_the_tree_it_reports_on_does_not_verify() {
        let mut ledger = EvidenceLedger::default();
        // One call that both mutated and exited zero: the tree it measured is
        // not the tree it left behind.
        ledger.record_call(true, Some(0));
        assert_eq!(ledger.status(), EvidenceStatus::Unverified);
    }

    #[test]
    fn an_unobservable_worktree_is_unknown_rather_than_clean() {
        let mut ledger = EvidenceLedger::default();
        ledger.mark_fingerprint_unavailable();
        assert_eq!(
            ledger.status(),
            EvidenceStatus::Unknown,
            "never report absence of mutations we could not look for"
        );
    }

    #[test]
    fn an_observed_mutation_survives_a_later_fingerprint_failure() {
        let mut ledger = EvidenceLedger::default();
        ledger.record_call(true, None);
        ledger.mark_fingerprint_unavailable();
        assert_eq!(
            ledger.status(),
            EvidenceStatus::Unverified,
            "the tree is known to have changed, so the signal is still usable"
        );
    }

    #[test]
    fn repeated_verifiers_keep_the_newest_position() {
        let mut ledger = EvidenceLedger::default();
        ledger.record_call(false, Some(0));
        ledger.record_call(true, None);
        ledger.record_call(false, Some(0));
        assert_eq!(ledger.status(), EvidenceStatus::Verified);
    }

    #[test]
    fn verifier_exit_reads_test_runner_status() {
        assert_eq!(
            verifier_exit("exec", &json!({"command": "pytest -q"}), r#"{"exit":0}"#),
            Some(0)
        );
        assert_eq!(
            verifier_exit("exec", &json!({"command": "cargo test"}), r#"{"exit":101}"#),
            Some(101)
        );
    }

    #[test]
    fn verifier_exit_rejects_non_verifier_shapes() {
        // Not a verifier command.
        assert_eq!(
            verifier_exit("exec", &json!({"command": "ls -la"}), r#"{"exit":0}"#),
            None
        );
        // Installs are excluded on purpose.
        assert_eq!(
            verifier_exit(
                "exec",
                &json!({"command": "pip install x"}),
                r#"{"exit":0}"#
            ),
            None
        );
        // Not the exec tool.
        assert_eq!(
            verifier_exit("read_file", &json!({"path": "pytest"}), r#"{"exit":0}"#),
            None
        );
        // Unparseable or exit-less result.
        assert_eq!(
            verifier_exit("exec", &json!({"command": "pytest"}), "not json"),
            None
        );
        assert_eq!(
            verifier_exit("exec", &json!({"command": "pytest"}), r#"{"out":"hi"}"#),
            None
        );
        // A shell's final success must not launder an earlier verifier failure.
        assert_eq!(
            verifier_exit(
                "exec",
                &json!({"command": "pytest -q; true"}),
                r#"{"exit":0}"#
            ),
            None
        );
        assert_eq!(
            verifier_exit(
                "exec",
                &json!({"command": "pytest -q | tee test.log"}),
                r#"{"exit":0}"#
            ),
            None
        );
    }

    #[tokio::test]
    async fn fingerprint_changes_only_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());

        let first = tree.fingerprint().await.expect("git available");
        let unchanged = tree.fingerprint().await.unwrap();
        assert_eq!(first, unchanged, "an untouched tree hashes identically");

        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        let changed = tree.fingerprint().await.unwrap();
        assert_ne!(first, changed, "edited content must change the hash");

        // Re-editing an already-dirty file must still register. A status-based
        // fingerprint would report " M a.txt" both times and miss this.
        std::fs::write(dir.path().join("a.txt"), "three").unwrap();
        assert_ne!(changed, tree.fingerprint().await.unwrap());

        // Reverting returns to the original hash: the tree really is unchanged.
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        assert_eq!(first, tree.fingerprint().await.unwrap());
    }

    #[tokio::test]
    async fn fingerprint_includes_gitignored_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "ignored.txt
",
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
        let first = tree.fingerprint().await.unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "two").unwrap();
        assert_ne!(first, tree.fingerprint().await.unwrap());
    }

    #[tokio::test]
    async fn concurrent_fingerprinters_use_independent_shadow_repositories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let left = Worktree::new(dir.path().to_path_buf(), state.path());
        let right = Worktree::new(dir.path().to_path_buf(), state.path());
        assert_ne!(left.git_dir, right.git_dir);
        let (a, b) = tokio::join!(left.fingerprint(), right.fingerprint());
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn shadow_repository_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let path;
        {
            let tree = Worktree::new(dir.path().to_path_buf(), state.path());
            path = tree.git_dir.clone();
            assert!(tree.fingerprint().await.is_some());
            assert!(path.exists());
        }
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn fingerprint_works_in_a_workspace_that_is_not_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "no repo here").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
        assert!(
            tree.fingerprint().await.is_some(),
            "the shadow repo is ours, so the workspace needs no git of its own"
        );
    }
}
