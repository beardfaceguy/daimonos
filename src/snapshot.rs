use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SNAPSHOTS_DIR: &str = ".daimonos/snapshots";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub id: String,
    pub tag: Option<String>,
    pub created: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

pub struct SnapshotStore {
    workspace: PathBuf,
}

impl SnapshotStore {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.workspace.join(SNAPSHOTS_DIR)
    }

    fn snap_dir(&self, id: &str) -> PathBuf {
        self.snapshots_dir().join(id)
    }

    /// Create a snapshot of the workspace.
    /// Copies all tracked files (respecting .gitignore), preserving relative paths.
    pub async fn create(&self, tag: Option<String>) -> Result<SnapshotMeta, String> {
        let id = Uuid::new_v4().to_string();
        let snap_dir = self.snap_dir(&id);
        let files_dir = snap_dir.join("files");

        std::fs::create_dir_all(&files_dir).map_err(|e| format!("mkdir: {e}"))?;

        let workspace = self.workspace.clone();
        let files_dir_clone = files_dir.clone();

        let (file_count, total_bytes) =
            tokio::task::spawn_blocking(move || copy_workspace(&workspace, &files_dir_clone))
                .await
                .map_err(|e| format!("spawn: {e}"))??;

        let now = chrono_now();
        let meta = SnapshotMeta {
            id,
            tag,
            created: now,
            file_count,
            total_bytes,
        };

        let manifest_path = snap_dir.join(MANIFEST_FILE);
        let json = serde_json::to_string_pretty(&meta).map_err(|e| format!("json: {e}"))?;
        std::fs::write(&manifest_path, json).map_err(|e| format!("write manifest: {e}"))?;

        Ok(meta)
    }

    /// Restore a snapshot, replacing workspace files with the snapshot's contents.
    /// Removes files not in the snapshot that were in the workspace (tracked files only).
    pub async fn restore(&self, id: &str) -> Result<SnapshotMeta, String> {
        let snap_dir = self.snap_dir(id);
        if !snap_dir.exists() {
            return Err(format!("snapshot not found: {id}"));
        }

        let meta = self.read_manifest(id)?;
        let files_dir = snap_dir.join("files");

        let workspace = self.workspace.clone();
        let files_dir_clone = files_dir.clone();

        tokio::task::spawn_blocking(move || restore_workspace(&workspace, &files_dir_clone))
            .await
            .map_err(|e| format!("spawn: {e}"))??;

        Ok(meta)
    }

    /// List all snapshots, newest first.
    pub fn list(&self) -> Result<Vec<SnapshotMeta>, String> {
        let dir = self.snapshots_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut snaps = Vec::new();
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("readdir: {e}"))?;

        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let id = entry.file_name().to_string_lossy().to_string();
                if let Ok(meta) = self.read_manifest(&id) {
                    snaps.push(meta);
                }
            }
        }

        snaps.sort_by(|a, b| b.created.cmp(&a.created));
        Ok(snaps)
    }

    /// Delete a snapshot.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let snap_dir = self.snap_dir(id);
        if !snap_dir.exists() {
            return Err(format!("snapshot not found: {id}"));
        }
        std::fs::remove_dir_all(&snap_dir).map_err(|e| format!("delete: {e}"))
    }

    fn read_manifest(&self, id: &str) -> Result<SnapshotMeta, String> {
        let path = self.snap_dir(id).join(MANIFEST_FILE);
        let content = std::fs::read_to_string(&path).map_err(|e| format!("read manifest: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("parse manifest: {e}"))
    }
}

fn copy_workspace(workspace: &Path, dest: &Path) -> Result<(usize, u64), String> {
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;

    let walker = WalkBuilder::new(workspace)
        .git_ignore(true)
        .hidden(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            name != ".daimonos" && name != ".git"
        })
        .build();

    for result in walker {
        let entry = result.map_err(|e| format!("walk: {e}"))?;
        let path = entry.path();

        let rel = path
            .strip_prefix(workspace)
            .map_err(|e| format!("strip: {e}"))?;

        if rel.as_os_str().is_empty() {
            continue;
        }

        let target = dest.join(rel);

        if path.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("mkdir {}: {e}", rel.display()))?;
        } else if path.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
            }
            let bytes =
                std::fs::copy(path, &target).map_err(|e| format!("copy {}: {e}", rel.display()))?;
            file_count += 1;
            total_bytes += bytes;
        }
    }

    Ok((file_count, total_bytes))
}

fn restore_workspace(workspace: &Path, snap_files: &Path) -> Result<(), String> {
    let snap_set = collect_relative_paths(snap_files)?;
    let workspace_set = collect_workspace_paths(workspace)?;

    // Remove workspace files not in the snapshot
    for (rel, path) in &workspace_set {
        if !snap_set.contains_key(rel) && path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }

    // Copy snapshot files to workspace
    for (rel, src) in &snap_set {
        let dest = workspace.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        std::fs::copy(src, &dest).map_err(|e| format!("restore {}: {e}", rel))?;
    }

    // Clean up empty directories
    clean_empty_dirs(workspace);

    Ok(())
}

fn collect_relative_paths(root: &Path) -> Result<HashMap<String, PathBuf>, String> {
    let mut map = HashMap::new();
    let walker = WalkBuilder::new(root)
        .git_ignore(false)
        .hidden(false)
        .build();

    for result in walker {
        let entry = result.map_err(|e| format!("walk: {e}"))?;
        let path = entry.path();
        if path.is_file() {
            let rel = path.strip_prefix(root).map_err(|e| format!("strip: {e}"))?;
            map.insert(rel.to_string_lossy().to_string(), path.to_path_buf());
        }
    }

    Ok(map)
}

fn collect_workspace_paths(workspace: &Path) -> Result<HashMap<String, PathBuf>, String> {
    let mut map = HashMap::new();
    let walker = WalkBuilder::new(workspace)
        .git_ignore(true)
        .hidden(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            name != ".daimonos" && name != ".git"
        })
        .build();

    for result in walker {
        let entry = result.map_err(|e| format!("walk: {e}"))?;
        let path = entry.path();
        if path.is_file() {
            let rel = path
                .strip_prefix(workspace)
                .map_err(|e| format!("strip: {e}"))?;
            map.insert(rel.to_string_lossy().to_string(), path.to_path_buf());
        }
    }

    Ok(map)
}

fn clean_empty_dirs(root: &Path) {
    let mut dirs: Vec<PathBuf> = Vec::new();

    for e in walkdir::WalkDir::new(root)
        .min_depth(1)
        .contents_first(true)
        .into_iter()
        .flatten()
    {
        let name = e.file_name().to_string_lossy();
        if name == ".git" || name == ".daimonos" {
            continue;
        }
        if e.file_type().is_dir() {
            dirs.push(e.path().to_path_buf());
        }
    }

    for dir in dirs {
        let _ = std::fs::remove_dir(&dir); // only succeeds if empty
    }
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_workspace(dir: &Path) {
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn hello() {}").unwrap();
        std::fs::write(dir.join("README.md"), "# Test").unwrap();
    }

    #[tokio::test]
    async fn create_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let meta = store.create(Some("v1".into())).await.unwrap();

        assert_eq!(meta.tag, Some("v1".into()));
        assert_eq!(meta.file_count, 3);
        assert!(meta.total_bytes > 0);
        assert!(!meta.id.is_empty());

        let snap_dir = dir.path().join(".daimonos/snapshots").join(&meta.id);
        assert!(snap_dir.join("manifest.json").exists());
        assert!(snap_dir.join("files/main.rs").exists());
        assert!(snap_dir.join("files/src/lib.rs").exists());
    }

    #[tokio::test]
    async fn create_without_tag() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let meta = store.create(None).await.unwrap();
        assert_eq!(meta.tag, None);
        assert_eq!(meta.file_count, 3);
    }

    #[tokio::test]
    async fn restore_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let meta = store.create(Some("before-edit".into())).await.unwrap();

        // Modify workspace
        std::fs::write(dir.path().join("main.rs"), "fn main() { panic!() }").unwrap();
        std::fs::write(dir.path().join("new_file.txt"), "extra").unwrap();

        let content_before_restore = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        assert!(content_before_restore.contains("panic"));

        // Restore
        let restored = store.restore(&meta.id).await.unwrap();
        assert_eq!(restored.id, meta.id);

        let content_after = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        assert_eq!(content_after, "fn main() {}");

        // new_file.txt should be gone
        assert!(!dir.path().join("new_file.txt").exists());
    }

    #[tokio::test]
    async fn restore_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        let result = store.restore("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        assert_eq!(store.list().unwrap().len(), 0);

        store.create(Some("first".into())).await.unwrap();
        store.create(Some("second".into())).await.unwrap();

        let snaps = store.list().unwrap();
        assert_eq!(snaps.len(), 2);
    }

    #[tokio::test]
    async fn delete_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let meta = store.create(Some("temp".into())).await.unwrap();

        assert_eq!(store.list().unwrap().len(), 1);
        store.delete(&meta.id).unwrap();
        assert_eq!(store.list().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        let result = store.delete("nope");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn snapshot_skips_daimonos_dir() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let first = store.create(Some("first".into())).await.unwrap();
        // Creating a second snapshot should not capture .daimonos/snapshots
        let second = store.create(Some("second".into())).await.unwrap();
        assert_eq!(second.file_count, first.file_count);
    }

    #[tokio::test]
    async fn snapshot_skips_gitignored_files() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        // Initialize git repo so .gitignore is respected
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join(".gitignore"), "build/\n*.log\n").unwrap();
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build/output.o"), "binary").unwrap();
        std::fs::write(dir.path().join("debug.log"), "log stuff").unwrap();

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let meta = store.create(None).await.unwrap();

        // 3 original files + .gitignore = 4, build/ and .log should be skipped
        assert_eq!(meta.file_count, 4);
    }

    #[tokio::test]
    async fn restore_removes_added_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let meta = store.create(None).await.unwrap();

        // Add new directory with files
        std::fs::create_dir_all(dir.path().join("extra/nested")).unwrap();
        std::fs::write(dir.path().join("extra/nested/deep.txt"), "deep").unwrap();

        assert!(dir.path().join("extra/nested/deep.txt").exists());

        store.restore(&meta.id).await.unwrap();

        assert!(!dir.path().join("extra/nested/deep.txt").exists());
        assert!(!dir.path().join("extra").exists());
    }

    #[tokio::test]
    async fn multiple_snapshots_independent() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());

        let store = SnapshotStore::new(dir.path().to_path_buf());
        let snap_v1 = store.create(Some("v1".into())).await.unwrap();

        std::fs::write(dir.path().join("main.rs"), "fn main() { v2() }").unwrap();
        let snap_v2 = store.create(Some("v2".into())).await.unwrap();

        // Restore v1
        store.restore(&snap_v1.id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "fn main() {}"
        );

        // Restore v2
        store.restore(&snap_v2.id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("main.rs")).unwrap(),
            "fn main() { v2() }"
        );
    }
}
