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
//! hash of every workspace entry's metadata (path, type, size, mtime, inode),
//! including ignored content, with content hashing only for files written in
//! the last moments (ADR-035, #1617). It writes nothing and never touches the
//! user's index. It replaced a per-turn git shadow repository that copied the
//! whole tree, build output included.
//!
//! When the fingerprint is unavailable the status is [`EvidenceStatus::Unknown`]
//! rather than "no mutations" — reporting unobserved absence as evidence is the
//! exact failure this ledger exists to prevent.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// Files modified this recently before a scan also get their content hashed.
/// Filesystem timestamps come from a coarse clock (a few ms on Linux, up to
/// 2 s on FAT), so two quick same-size writes can share an mtime. Git calls
/// this the "racy" case and solves it the same way.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Workspace fingerprint built from file metadata (vikunja #1617, ADR-035).
///
/// A scan records each entry's relative path, type, size, mtime, and (on
/// Unix) inode and mode, plus a symlink's target. Ignored files count: an
/// agent can change a generated or secret-bearing ignored file. Only `.git`
/// and `.daimonos` directories are skipped. Reading metadata takes about
/// 0.1 s for 43k files; the former shadow-repo snapshot copied every byte
/// (about 3 min and 8 GB for a 21 GB `target/`).
///
/// A revert can count as a change, because its mtime moved even when its
/// bytes match again. That errs toward an "unverified" warning, never toward
/// a missed change.
pub struct Worktree {
    workspace: PathBuf,
    legacy_store: PathBuf,
}

impl Worktree {
    /// Build a fingerprinter for `workspace`. `state_base` (normally
    /// `~/.daimonos`) only locates the legacy shadow-repo store to remove.
    pub fn new(workspace: impl Into<PathBuf>, state_base: &Path) -> Self {
        Self {
            workspace: workspace.into(),
            legacy_store: state_base.join("evidence"),
        }
    }

    /// Metadata fingerprint of the current worktree, or `None` when the
    /// workspace cannot be read. Never an error: absent evidence is reported
    /// as absent, not as a failed turn.
    pub async fn fingerprint(&self) -> Option<String> {
        remove_legacy_store_once(&self.legacy_store);
        let workspace = self.workspace.clone();
        tokio::task::spawn_blocking(move || scan(&workspace))
            .await
            .ok()?
    }
}

fn scan(workspace: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let racy_since = SystemTime::now()
        .checked_sub(RACY_WINDOW)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut hasher = Sha256::new();
    let walker = walkdir::WalkDir::new(workspace)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !matches!(entry.file_name().to_str(), Some(".git" | ".daimonos"))
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            // An unreadable workspace root means nothing was observed.
            Err(error) if error.depth() == 0 => return None,
            // An unreadable subtree is part of the observed state.
            Err(error) => {
                hasher.update(b"E\0");
                if let Some(path) = error.path() {
                    hasher.update(path.as_os_str().as_encoded_bytes());
                }
                hasher.update(b"\0");
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry.path().strip_prefix(workspace).ok()?;
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        match entry.metadata() {
            Ok(metadata) => record_entry(&mut hasher, entry.path(), &metadata, racy_since),
            Err(_) => hasher.update(b"E"),
        }
        hasher.update(b"\n");
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

fn record_entry(
    hasher: &mut sha2::Sha256,
    path: &Path,
    metadata: &std::fs::Metadata,
    racy_since: SystemTime,
) {
    use sha2::Digest;
    let file_type = metadata.file_type();
    let kind: &[u8] = if file_type.is_dir() {
        b"d"
    } else if file_type.is_symlink() {
        b"l"
    } else if file_type.is_file() {
        b"f"
    } else {
        b"o"
    };
    hasher.update(kind);
    if file_type.is_dir() {
        return; // its entries are recorded individually
    }
    hasher.update(metadata.len().to_le_bytes());
    let modified = metadata.modified().ok();
    let mtime_ns = modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    hasher.update(mtime_ns.to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hasher.update(metadata.ino().to_le_bytes());
        hasher.update(metadata.mode().to_le_bytes());
    }
    if file_type.is_symlink() {
        if let Ok(target) = std::fs::read_link(path) {
            hasher.update(target.as_os_str().as_encoded_bytes());
        }
    } else if file_type.is_file() && modified.is_none_or(|time| time >= racy_since) {
        // Racy entry: hash its bytes so a same-tick, same-size rewrite shows.
        match std::fs::File::open(path) {
            Ok(mut file) => {
                if std::io::copy(&mut file, hasher).is_err() {
                    hasher.update(b"E");
                }
            }
            Err(_) => hasher.update(b"E"),
        }
    }
}

/// Remove the shadow-repo store that earlier versions wrote per turn
/// (`<state>/evidence`). Killed turns left multi-GB repos there (62 GB on one
/// machine, #1617). Nothing writes it any more, so it is deleted once per
/// process, in the background. A turn of an older build that is still running
/// loses its snapshot; its evidence becomes `Unknown`, which is advisory.
fn remove_legacy_store_once(store: &Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let store = store.to_path_buf();
        tokio::task::spawn_blocking(move || remove_legacy_store(&store));
    });
}

pub(crate) fn remove_legacy_store(store: &Path) {
    if !store.exists() {
        return;
    }
    let removed = std::fs::remove_dir_all(store).is_ok();
    tracing::info!(
        target: "daimonos::evidence",
        event = "evidence_legacy_store_removed",
        store = %store.display(),
        removed,
    );
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

    /// #1617 / ADR-035: the fingerprint is a metadata scan. It must not copy
    /// the workspace anywhere, and a same-size rewrite inside one coarse clock
    /// tick (identical size and mtime) must still register.
    #[tokio::test]
    async fn metadata_fingerprint_writes_nothing_and_sees_same_tick_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());

        let before = tree.fingerprint().await.expect("scan succeeds");
        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        let after = tree.fingerprint().await.unwrap();

        assert_ne!(
            before, after,
            "a same-size rewrite must change the fingerprint"
        );
        let written: Vec<_> = walkdir::WalkDir::new(state.path())
            .min_depth(1)
            .into_iter()
            .collect();
        assert!(
            written.is_empty(),
            "no copies under the state dir: {written:?}"
        );
    }

    #[tokio::test]
    async fn fingerprint_changes_only_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());

        let first = tree.fingerprint().await.expect("scan succeeds");
        let unchanged = tree.fingerprint().await.unwrap();
        assert_eq!(first, unchanged, "an untouched tree hashes identically");

        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        let changed = tree.fingerprint().await.unwrap();
        assert_ne!(first, changed, "edited content must change the hash");

        // Re-editing an already-dirty file must still register. A status-based
        // fingerprint would report " M a.txt" both times and miss this.
        std::fs::write(dir.path().join("a.txt"), "three").unwrap();
        assert_ne!(changed, tree.fingerprint().await.unwrap());

        // A revert is not asserted either way: ADR-035 lets it count as a
        // change, because the file's mtime moved even when its bytes match.
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
    async fn concurrent_fingerprints_of_one_tree_agree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let left = Worktree::new(dir.path().to_path_buf(), state.path());
        let right = Worktree::new(dir.path().to_path_buf(), state.path());
        let (a, b) = tokio::join!(left.fingerprint(), right.fingerprint());
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn created_and_deleted_files_change_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
        let base = tree.fingerprint().await.unwrap();
        std::fs::write(dir.path().join("b.txt"), "new").unwrap();
        let created = tree.fingerprint().await.unwrap();
        assert_ne!(base, created, "a new file must register");
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        assert_ne!(
            created,
            tree.fingerprint().await.unwrap(),
            "a deletion must register"
        );
    }

    #[tokio::test]
    async fn a_missing_workspace_is_unobservable() {
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(state.path().join("does-not-exist"), state.path());
        assert_eq!(tree.fingerprint().await, None);
    }

    /// #1617: earlier versions left multi-GB per-turn shadow repos under
    /// `<state>/evidence` when a turn was killed. The new scan writes nothing
    /// there, so the whole legacy store is removed.
    #[test]
    fn legacy_shadow_store_is_removed() {
        let state = tempfile::tempdir().unwrap();
        let store = state.path().join("evidence");
        std::fs::create_dir_all(store.join("abc").join("dead.git").join("objects")).unwrap();
        std::fs::write(store.join("abc").join("dead.lock"), "").unwrap();

        remove_legacy_store(&store);

        assert!(!store.exists());
        remove_legacy_store(&store); // absent store is a no-op
    }

    #[tokio::test]
    async fn fingerprint_works_in_a_workspace_that_is_not_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "no repo here").unwrap();
        let state = tempfile::tempdir().unwrap();
        let tree = Worktree::new(dir.path().to_path_buf(), state.path());
        assert!(
            tree.fingerprint().await.is_some(),
            "the scan reads the workspace directly, so it needs no git of its own"
        );
    }
}
