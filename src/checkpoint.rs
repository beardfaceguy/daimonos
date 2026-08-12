//! Automatic per-turn workspace checkpoints backed by a shadow git repository
//! (vikunja #1239, adapted from Cline's shadow-git checkpoints).
//!
//! # Why shadow git rather than copying
//!
//! `snapshot.rs` copies the whole workspace per snapshot. Measured on daimonos
//! itself (328 files / 12.1 MiB), that costs 13 ms — cheap enough per turn on
//! *latency* — but **257 MB of disk for 20 checkpoints**, because every
//! checkpoint is a full copy. The same 20 checkpoints in a shadow git repo cost
//! **12 MB**: git content-addresses blobs, so an unchanged file is stored once
//! no matter how many checkpoints reference it.
//!
//! Hardlinking would be faster still (2.9 ms) and nearly free on disk, but it is
//! **incorrect here**: `ops::file_ops` writes with `tokio::fs::write`, which
//! truncates the existing inode in place instead of writing a temp file and
//! renaming. A hardlinked checkpoint shares that inode, so the next write to a
//! path would silently rewrite the checkpoint's copy of it — checkpoints that
//! look fine and are quietly corrupt. Git reads content at commit time, so it is
//! immune to how the file was written.
//!
//! # Isolation
//!
//! The shadow repo lives at `.daimonos/checkpoints.git` and is driven purely via
//! `--git-dir` + `--work-tree`, so:
//!
//! - the project's real `.git` is never read or written (git skips any directory
//!   named `.git` while scanning a work tree);
//! - the project's `.gitignore` is honoured, so build output stays out;
//! - `.daimonos/` is excluded via the shadow repo's `info/exclude`, so the
//!   checkpoint store never checkpoints itself;
//! - the workspace does not need to be a git repository at all.

// `diff` and `restore_files` are the operator-facing half: exercised by tests
// and reached from the `checkpoint` tool. Kept public here so the store is one
// coherent API rather than being split by what the loop happens to call.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;

/// Directory of the shadow repository, relative to the workspace.
const SHADOW_DIR: &str = ".daimonos/checkpoints.git";

/// Identity used for shadow commits. The user's git identity may be unset, and
/// committing would fail; these values never reach the project's real history.
const COMMIT_NAME: &str = "daimonos";
const COMMIT_EMAIL: &str = "daimonos@localhost";

/// One recorded checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Shadow-repo commit hash; the handle for diff and restore.
    pub id: String,
    /// Human label, e.g. the tool call that triggered it.
    pub label: String,
    /// Commit timestamp, RFC 3339.
    pub created_at: String,
}

/// Outcome of a files-only restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Checkpoint restored to.
    pub id: String,
    /// Checkpoint of the pre-restore state, so the restore itself is undoable.
    pub undo_id: String,
    /// Paths whose content changed as a result.
    pub changed: Vec<String>,
}

pub struct CheckpointStore {
    workspace: PathBuf,
    git_dir: PathBuf,
}

impl CheckpointStore {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let git_dir = workspace.join(SHADOW_DIR);
        Self { workspace, git_dir }
    }

    /// Run one git command against the shadow repo. Never inherits the ambient
    /// git environment: `--git-dir`/`--work-tree` are always explicit so a
    /// stray `GIT_DIR` cannot redirect a checkpoint into the real repository.
    async fn git(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.workspace)
            // Identity on the command line, not in config: the shadow repo is
            // disposable and the user may have no global identity set.
            .args(["-c", &format!("user.name={COMMIT_NAME}")])
            .args(["-c", &format!("user.email={COMMIT_EMAIL}")])
            .args(args)
            .current_dir(&self.workspace)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null());
        let out = cmd
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

    /// Create the shadow repository if absent. Idempotent.
    pub async fn init(&self) -> Result<(), String> {
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
        // Exclude our own store, or every checkpoint would contain the previous
        // ones and the repo would grow quadratically.
        let info = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info)
            .await
            .map_err(|e| format!("create info: {e}"))?;
        tokio::fs::write(info.join("exclude"), ".daimonos/\n")
            .await
            .map_err(|e| format!("write exclude: {e}"))?;
        Ok(())
    }

    /// Record the current workspace state. Returns `None` when nothing changed
    /// since the previous checkpoint — an unchanged turn should not produce a
    /// checkpoint the user has to scroll past.
    pub async fn create(&self, label: &str) -> Result<Option<Checkpoint>, String> {
        self.init().await?;
        self.git(&["add", "-A"]).await?;
        // `diff --cached --quiet` exits 1 when staged changes exist, so a
        // successful (exit 0) run means there is nothing to commit.
        if self.git(&["diff", "--cached", "--quiet"]).await.is_ok() && self.head().await?.is_some()
        {
            return Ok(None);
        }
        self.git(&["commit", "--quiet", "--no-verify", "-m", label])
            .await?;
        let id = self
            .head()
            .await?
            .ok_or_else(|| "commit produced no HEAD".to_string())?;
        Ok(Some(Checkpoint {
            id,
            label: label.to_string(),
            created_at: self
                .git(&["log", "-1", "--format=%cI"])
                .await?
                .trim()
                .into(),
        }))
    }

    async fn head(&self) -> Result<Option<String>, String> {
        match self.git(&["rev-parse", "HEAD"]).await {
            Ok(v) => Ok(Some(v.trim().to_string())),
            // No commits yet: not an error, just an empty history.
            Err(_) => Ok(None),
        }
    }

    /// Checkpoints, newest first.
    pub async fn list(&self) -> Result<Vec<Checkpoint>, String> {
        if !self.git_dir.join("HEAD").exists() {
            return Ok(Vec::new());
        }
        let Some(_) = self.head().await? else {
            return Ok(Vec::new());
        };
        let raw = self.git(&["log", "--format=%H%x1f%s%x1f%cI"]).await?;
        Ok(raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| {
                let mut parts = line.split('\u{1f}');
                Some(Checkpoint {
                    id: parts.next()?.to_string(),
                    label: parts.next()?.to_string(),
                    created_at: parts.next()?.to_string(),
                })
            })
            .collect())
    }

    /// Unified diff between two checkpoints, or between one and the current
    /// working tree when `to` is `None`.
    pub async fn diff(&self, from: &str, to: Option<&str>) -> Result<String, String> {
        match to {
            Some(to) => self.git(&["diff", from, to]).await,
            None => self.git(&["diff", from]).await,
        }
    }

    /// Restore workspace **files** to a checkpoint, leaving conversation state
    /// untouched — the point of the feature: undo bad edits cheaply without
    /// losing the session.
    ///
    /// Transactional in the sense that matters: the pre-restore state is
    /// checkpointed *first*, so a restore is itself undoable and a failure
    /// midway leaves a recorded state to return to rather than a half-reverted
    /// tree with no record of what was lost.
    pub async fn restore_files(&self, id: &str) -> Result<RestoreOutcome, String> {
        self.init().await?;
        // Verify the target before touching anything.
        self.git(&["cat-file", "-e", &format!("{id}^{{commit}}")])
            .await
            .map_err(|_| format!("unknown checkpoint {id}"))?;

        let undo = self
            .create(&format!("pre-restore of {}", short(id)))
            .await?
            .map(|c| c.id)
            .or(self.head().await?)
            .ok_or_else(|| "cannot record pre-restore state".to_string())?;

        let changed = self
            .git(&["diff", "--name-only", id])
            .await?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();

        // `read-tree -u --reset`, not `checkout -- .`: checkout restores the
        // files present in `id` but leaves behind files added *since* it, which
        // are tracked in HEAD and therefore untouched by `git clean` too. That
        // is not a restore. read-tree makes index and work tree match the tree
        // exactly — including deletions — without moving HEAD, so the earlier
        // checkpoints stay on the branch and the restore records as a new
        // commit on top rather than rewriting history.
        self.git(&["read-tree", "-u", "--reset", id]).await?;
        // Untracked leftovers (files never checkpointed). Ignore rules still
        // apply, so build output is never touched.
        self.git(&["clean", "-fdq"]).await?;

        Ok(RestoreOutcome {
            id: id.to_string(),
            undo_id: undo,
            changed,
        })
    }

    /// Drop all but the `keep` newest checkpoints, then repack so the space is
    /// actually reclaimed. Returns how many were dropped.
    ///
    /// Rewrites history by re-rooting at the oldest kept checkpoint: git cannot
    /// delete a commit that later commits descend from, so trimming the tail
    /// means grafting. Checkpoints are disposable per-turn state, not project
    /// history, so rewriting them is safe by construction.
    pub async fn gc(&self, keep: usize) -> Result<usize, String> {
        let all = self.list().await?;
        if keep == 0 || all.len() <= keep {
            return Ok(0);
        }
        let dropped = all.len() - keep;
        let oldest_kept = &all[keep - 1];
        // Re-root: make the oldest kept checkpoint parentless, orphaning the
        // tail, then expire reflogs and prune so the objects are collectable.
        self.git(&["replace", "--graft", &oldest_kept.id]).await?;
        self.git(&["filter-branch", "--force", "--", "--all"])
            .await
            .ok();
        self.git(&["replace", "-d", &oldest_kept.id]).await.ok();
        self.git(&["reflog", "expire", "--expire=now", "--all"])
            .await
            .ok();
        self.git(&["gc", "--prune=now", "--quiet"]).await.ok();
        Ok(dropped)
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn workspace() -> (tempfile::TempDir, CheckpointStore) {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".gitignore"), "build/\n")
            .await
            .unwrap();
        let store = CheckpointStore::new(dir.path());
        (dir, store)
    }

    #[tokio::test]
    async fn create_records_a_checkpoint_and_list_returns_it() {
        let (dir, store) = workspace().await;
        let cp = store.create("first").await.unwrap().expect("checkpoint");
        assert_eq!(cp.label, "first");
        assert!(!cp.id.is_empty());

        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, cp.id);
        // The store lives inside the workspace it checkpoints.
        assert!(dir.path().join(SHADOW_DIR).join("HEAD").exists());
    }

    /// An unchanged turn must not produce a checkpoint — otherwise a long
    /// session fills the list with identical entries the user has to scroll past.
    #[tokio::test]
    async fn an_unchanged_workspace_produces_no_new_checkpoint() {
        let (_dir, store) = workspace().await;
        store.create("first").await.unwrap().expect("first");
        assert!(
            store.create("second").await.unwrap().is_none(),
            "nothing changed, so nothing to record"
        );
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    /// The three isolation properties, together: the real repo is untouched,
    /// gitignored paths stay out, and the store never checkpoints itself.
    #[tokio::test]
    async fn checkpoints_ignore_the_real_repo_gitignored_paths_and_the_store() {
        let (dir, store) = workspace().await;
        // A real repository with its own history.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        git(&["init", "--quiet"]);
        git(&["-c", "user.email=r@x", "-c", "user.name=r", "add", "-A"]);
        git(&[
            "-c",
            "user.email=r@x",
            "-c",
            "user.name=r",
            "commit",
            "-qm",
            "real",
        ]);
        let real_head_before = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();

        tokio::fs::create_dir_all(dir.path().join("build"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("build/out.o"), "binary")
            .await
            .unwrap();

        store.create("cp").await.unwrap().expect("checkpoint");

        let tracked = store
            .git(&["ls-tree", "-r", "--name-only", "HEAD"])
            .await
            .unwrap();
        assert!(tracked.contains("src/a.rs"), "source is checkpointed");
        assert!(
            !tracked.contains("build/out.o"),
            "gitignored output excluded"
        );
        assert!(!tracked.contains(".daimonos"), "the store excludes itself");
        assert!(
            !tracked.contains(".git/"),
            "the real repo is not checkpointed"
        );

        let real_head_after = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();
        assert_eq!(
            real_head_before, real_head_after,
            "the project's real history must be untouched"
        );
    }

    #[tokio::test]
    async fn diff_reports_what_changed_between_checkpoints() {
        let (dir, store) = workspace().await;
        let first = store.create("first").await.unwrap().unwrap();
        tokio::fs::write(dir.path().join("src/a.rs"), "fn a() { changed(); }\n")
            .await
            .unwrap();
        let second = store.create("second").await.unwrap().unwrap();

        let d = store.diff(&first.id, Some(&second.id)).await.unwrap();
        assert!(d.contains("src/a.rs"));
        assert!(d.contains("changed()"));
    }

    /// The headline behaviour: files roll back, and a file created after the
    /// checkpoint is removed — otherwise it is not a restore.
    #[tokio::test]
    async fn restore_rolls_files_back_and_removes_files_added_since() {
        let (dir, store) = workspace().await;
        let first = store.create("first").await.unwrap().unwrap();

        tokio::fs::write(dir.path().join("src/a.rs"), "fn a() { broken(); }\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/oops.rs"), "junk\n")
            .await
            .unwrap();
        store.create("bad").await.unwrap().unwrap();

        let outcome = store.restore_files(&first.id).await.unwrap();
        assert_eq!(outcome.id, first.id);

        let restored = tokio::fs::read_to_string(dir.path().join("src/a.rs"))
            .await
            .unwrap();
        assert_eq!(restored, "fn a() {}\n", "content rolled back");
        assert!(
            !dir.path().join("src/oops.rs").exists(),
            "a file added after the checkpoint must not survive a restore"
        );
    }

    /// A restore is itself undoable — the pre-restore state is checkpointed
    /// before anything is touched.
    #[tokio::test]
    async fn restore_is_undoable_via_its_pre_restore_checkpoint() {
        let (dir, store) = workspace().await;
        let first = store.create("first").await.unwrap().unwrap();
        tokio::fs::write(dir.path().join("src/a.rs"), "fn a() { work(); }\n")
            .await
            .unwrap();
        store.create("work").await.unwrap().unwrap();

        let outcome = store.restore_files(&first.id).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("src/a.rs"))
                .await
                .unwrap(),
            "fn a() {}\n"
        );

        // Undo the restore.
        store.restore_files(&outcome.undo_id).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("src/a.rs"))
                .await
                .unwrap(),
            "fn a() { work(); }\n",
            "the work the restore reverted is recoverable"
        );
    }

    #[tokio::test]
    async fn restore_rejects_an_unknown_checkpoint_without_touching_files() {
        let (dir, store) = workspace().await;
        store.create("first").await.unwrap().unwrap();
        tokio::fs::write(dir.path().join("src/a.rs"), "fn a() { edited(); }\n")
            .await
            .unwrap();

        let err = store
            .restore_files("0000000000000000000000000000000000000000")
            .await;
        assert!(err.is_err());
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("src/a.rs"))
                .await
                .unwrap(),
            "fn a() { edited(); }\n",
            "a failed restore must not have touched the tree"
        );
    }

    #[tokio::test]
    async fn gc_keeps_the_newest_checkpoints() {
        let (dir, store) = workspace().await;
        for i in 0..6 {
            tokio::fs::write(dir.path().join("src/a.rs"), format!("fn a() {{ {i} }}\n"))
                .await
                .unwrap();
            store.create(&format!("cp{i}")).await.unwrap().unwrap();
        }
        assert_eq!(store.list().await.unwrap().len(), 6);

        let dropped = store.gc(3).await.unwrap();
        assert_eq!(dropped, 3);
        let kept = store.list().await.unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].label, "cp5", "newest survives");

        assert_eq!(
            store.gc(10).await.unwrap(),
            0,
            "keeping more than exist is a no-op"
        );
    }

    #[tokio::test]
    async fn list_is_empty_before_anything_is_checkpointed() {
        let (_dir, store) = workspace().await;
        assert!(store.list().await.unwrap().is_empty());
    }
}
