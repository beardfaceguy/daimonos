use crate::config::IndexConfig;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// A trigram is a 3-byte sequence used for fast substring search.
type Trigram = [u8; 3];

/// Background workspace indexer.
/// Builds a trigram index over all text files for sub-millisecond search.
pub struct WorkspaceIndex {
    root: PathBuf,
    inner: Arc<RwLock<IndexState>>,
    max_depth: usize,
    max_file_size: usize,
    binary_sniff_bytes: usize,
    skip_extensions: HashSet<String>,
}

struct IndexState {
    /// Trigram -> set of file IDs that contain it
    trigrams: HashMap<Trigram, Vec<u32>>,
    /// File ID -> relative path
    files: Vec<String>,
    /// File ID -> last modified time (seconds since epoch)
    mtimes: Vec<u64>,
    /// Reverse lookup: relative path -> file ID
    path_to_id: HashMap<String, u32>,
    /// Total indexed files
    file_count: usize,
    /// Time of last index operation
    last_indexed: Option<Instant>,
}

impl IndexState {
    fn new() -> Self {
        Self {
            trigrams: HashMap::new(),
            files: Vec::new(),
            mtimes: Vec::new(),
            path_to_id: HashMap::new(),
            file_count: 0,
            last_indexed: None,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct IndexStats {
    pub files: usize,
    pub trigrams: usize,
    pub age_ms: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
pub struct SearchResult {
    pub file: String,
    pub score: u32,
}

impl WorkspaceIndex {
    pub fn new(root: PathBuf, cfg: &IndexConfig) -> Self {
        Self {
            root,
            inner: Arc::new(RwLock::new(IndexState::new())),
            max_depth: cfg.max_depth,
            max_file_size: cfg.max_file_size,
            binary_sniff_bytes: cfg.binary_sniff_bytes,
            skip_extensions: cfg.skip_set(),
        }
    }

    /// Rebuild the index in the background.
    /// On first call, does a full build. On subsequent calls, performs an
    /// incremental update: skips files whose mtime hasn't changed, removes
    /// deleted files, and only re-extracts trigrams for new/modified files.
    pub fn spawn_reindex(&self) {
        let root = self.root.clone();
        let inner = Arc::clone(&self.inner);
        let max_depth = self.max_depth;
        let max_file_size = self.max_file_size;
        let binary_sniff_bytes = self.binary_sniff_bytes;
        let skip_ext = self.skip_extensions.clone();

        tokio::task::spawn_blocking(move || {
            let start = Instant::now();

            // Snapshot the previous state. We're on a `spawn_blocking`
            // worker thread, so the tokio-recommended pattern is
            // `RwLock::blocking_read` rather than asking
            // `Handle::current()` to `block_on` a read future. The latter
            // works on the multi-threaded runtime but is fragile on the
            // current-thread flavor (the runtime's I/O driver and our
            // blocking worker live on different threads, and the round
            // trip through the scheduler is pure overhead).
            let prev = {
                let guard = inner.blocking_read();
                let prev_paths: HashMap<String, (u32, u64)> = guard
                    .path_to_id
                    .iter()
                    .map(|(path, &id)| {
                        let mtime = guard.mtimes.get(id as usize).copied().unwrap_or(0);
                        (path.clone(), (id, mtime))
                    })
                    .collect();
                let prev_trigrams = guard.trigrams.clone();
                let prev_files = guard.files.clone();
                (prev_paths, prev_trigrams, prev_files)
            };
            let (prev_paths, mut trigrams, _prev_files) = prev;
            let is_first_index = prev_paths.is_empty();

            // Walk the workspace and collect current file set
            let mut current_files: HashMap<String, u64> = HashMap::new();

            let walker = WalkBuilder::new(&root)
                .hidden(true)
                .git_ignore(true)
                .max_depth(Some(max_depth))
                .build();

            for entry in walker.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if skip_ext.contains(&ext) {
                    continue;
                }

                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();

                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| {
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                    })
                    .unwrap_or(0);

                current_files.insert(rel, mtime);
            }

            // Build new state incrementally
            let mut new_files: Vec<String> = Vec::new();
            let mut new_mtimes: Vec<u64> = Vec::new();
            let mut new_path_to_id: HashMap<String, u32> = HashMap::new();

            // If first index, clear trigrams; otherwise keep and we'll selectively update
            if is_first_index {
                trigrams.clear();
            }

            let mut skipped = 0usize;
            let mut updated = 0usize;
            let mut added = 0usize;

            // Identify deleted files — remove their trigram entries
            let deleted: Vec<u32> = prev_paths
                .iter()
                .filter(|(path, _)| !current_files.contains_key(path.as_str()))
                .map(|(_, (id, _))| *id)
                .collect();
            let deleted_set: HashSet<u32> = deleted.iter().copied().collect();
            let removed = deleted_set.len();

            if !deleted_set.is_empty() {
                for entries in trigrams.values_mut() {
                    entries.retain(|id| !deleted_set.contains(id));
                }
                trigrams.retain(|_, entries| !entries.is_empty());
            }

            // Old ID -> new ID mapping for files that survived
            let mut id_remap: HashMap<u32, u32> = HashMap::new();

            for (rel, &mtime) in &current_files {
                let file_id = new_files.len() as u32;

                if let Some(&(old_id, old_mtime)) = prev_paths.get(rel) {
                    if mtime == old_mtime && !is_first_index {
                        // Unchanged — keep existing trigrams, just remap the ID
                        id_remap.insert(old_id, file_id);
                        new_files.push(rel.clone());
                        new_mtimes.push(mtime);
                        new_path_to_id.insert(rel.clone(), file_id);
                        skipped += 1;
                        continue;
                    }
                    // Modified — remove old trigrams, re-extract
                    for entries in trigrams.values_mut() {
                        entries.retain(|id| *id != old_id);
                    }
                    updated += 1;
                } else {
                    added += 1;
                }

                // Read and index the file
                let path = root.join(rel);
                let content = match std::fs::read(&path) {
                    Ok(c) if c.len() <= max_file_size => c,
                    _ => continue,
                };

                if content.iter().take(binary_sniff_bytes).any(|&b| b == 0) {
                    continue;
                }

                new_files.push(rel.clone());
                new_mtimes.push(mtime);
                new_path_to_id.insert(rel.clone(), file_id);
                extract_trigrams(&content, file_id, &mut trigrams);
            }

            // Remap old file IDs to new file IDs in trigram entries
            if !id_remap.is_empty() {
                for entries in trigrams.values_mut() {
                    for id in entries.iter_mut() {
                        if let Some(&new_id) = id_remap.get(id) {
                            *id = new_id;
                        }
                    }
                }
            }

            // Clean up any empty trigram entries
            trigrams.retain(|_, entries| !entries.is_empty());

            let file_count = new_files.len();
            let trigram_count = trigrams.len();

            let state = IndexState {
                trigrams,
                files: new_files,
                mtimes: new_mtimes,
                path_to_id: new_path_to_id,
                file_count,
                last_indexed: Some(start),
            };

            // Same rationale as the snapshot read above: stay on the
            // blocking thread instead of bouncing through the runtime.
            {
                let mut guard = inner.blocking_write();
                *guard = state;
            }

            if is_first_index {
                eprintln!(
                    "index: {} files, {} trigrams in {:?}",
                    file_count,
                    trigram_count,
                    start.elapsed()
                );
            } else {
                eprintln!(
                    "index: {} files ({} skipped, {} updated, {} added, {} removed) in {:?}",
                    file_count,
                    skipped,
                    updated,
                    added,
                    removed,
                    start.elapsed()
                );
            }
        });
    }

    /// Search the trigram index for files likely containing the query.
    pub async fn search(&self, query: &str, max: usize) -> Vec<SearchResult> {
        let query_trigrams = query_to_trigrams(query.as_bytes());
        if query_trigrams.is_empty() {
            return Vec::new();
        }

        let guard = self.inner.read().await;

        let mut scores: HashMap<u32, u32> = HashMap::new();
        for tri in &query_trigrams {
            if let Some(file_ids) = guard.trigrams.get(tri) {
                for &fid in file_ids {
                    *scores.entry(fid).or_insert(0) += 1;
                }
            }
        }

        let threshold = (query_trigrams.len() as u32).saturating_sub(1).max(1);

        let mut results: Vec<SearchResult> = scores
            .into_iter()
            .filter(|(_, score)| *score >= threshold)
            .map(|(fid, score)| SearchResult {
                file: guard.files.get(fid as usize).cloned().unwrap_or_default(),
                score,
            })
            .collect();

        results.sort_by_key(|r| std::cmp::Reverse(r.score));
        results.truncate(max);
        results
    }

    pub async fn stats(&self) -> IndexStats {
        let guard = self.inner.read().await;
        IndexStats {
            files: guard.file_count,
            trigrams: guard.trigrams.len(),
            age_ms: guard.last_indexed.map(|t| t.elapsed().as_millis() as u64),
        }
    }
}

fn extract_trigrams(content: &[u8], file_id: u32, trigrams: &mut HashMap<Trigram, Vec<u32>>) {
    if content.len() < 3 {
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for window in content.windows(3) {
        let tri: Trigram = [window[0], window[1], window[2]];
        if seen.insert(tri) {
            trigrams.entry(tri).or_default().push(file_id);
        }
    }
}

fn query_to_trigrams(query: &[u8]) -> Vec<Trigram> {
    if query.len() < 3 {
        return Vec::new();
    }
    query.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_trigrams_from_content() {
        let mut trigrams = HashMap::new();
        extract_trigrams(b"abcde", 0, &mut trigrams);
        assert!(trigrams.contains_key(&[b'a', b'b', b'c']));
        assert!(trigrams.contains_key(&[b'b', b'c', b'd']));
        assert!(trigrams.contains_key(&[b'c', b'd', b'e']));
        assert_eq!(trigrams.len(), 3);
    }

    #[test]
    fn extract_trigrams_short_content() {
        let mut trigrams = HashMap::new();
        extract_trigrams(b"ab", 0, &mut trigrams);
        assert!(trigrams.is_empty());
    }

    #[test]
    fn extract_trigrams_deduplicates_per_file() {
        let mut trigrams = HashMap::new();
        extract_trigrams(b"aaaa", 0, &mut trigrams);
        assert_eq!(trigrams[&[b'a', b'a', b'a']].len(), 1);
    }

    #[test]
    fn query_to_trigrams_normal() {
        let tris = query_to_trigrams(b"hello");
        assert_eq!(tris.len(), 3);
        assert_eq!(tris[0], [b'h', b'e', b'l']);
    }

    #[test]
    fn query_to_trigrams_too_short() {
        assert!(query_to_trigrams(b"ab").is_empty());
        assert!(query_to_trigrams(b"").is_empty());
    }

    /// Regression for vikunja #253: the reindex worker used to call
    /// `Handle::current().block_on(...)` from inside `spawn_blocking`,
    /// which bounces the read/write futures back through the runtime
    /// scheduler. The pattern works on the multi-threaded runtime by
    /// accident but is fragile (and pure overhead) on the current-thread
    /// flavor. The fix moves to `RwLock::blocking_read`/`blocking_write`.
    ///
    /// This test pins the contract on both runtime flavors: spawn a
    /// reindex, await completion via `stats()`, and assert files were
    /// actually indexed. If a future change reintroduces the pattern
    /// (or a deadlock-prone variant), this test deadlocks/hangs/fails on
    /// at least one of the two flavors.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_works_on_multi_thread_runtime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha_marker() {}").unwrap();
        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);
        idx.spawn_reindex();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stats = idx.stats().await;
        assert_eq!(stats.files, 1, "multi-thread reindex did not complete");
        let hits = idx.search("alpha_marker", 10).await;
        assert!(!hits.is_empty(), "search after multi-thread reindex empty");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reindex_works_on_current_thread_runtime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn beta_marker() {}").unwrap();
        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);
        idx.spawn_reindex();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stats = idx.stats().await;
        assert_eq!(stats.files, 1, "current-thread reindex did not complete");
        let hits = idx.search("beta_marker", 10).await;
        assert!(!hits.is_empty(), "search after current-thread reindex empty");
    }

    #[tokio::test]
    async fn index_and_search_fixture_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "fn alpha_function() {}").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "some beta content here").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/gamma.rs"), "fn gamma_function() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);

        idx.spawn_reindex();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let stats = idx.stats().await;
        assert!(stats.files >= 3, "expected >= 3 files, got {}", stats.files);

        let results = idx.search("alpha_function", 10).await;
        assert!(!results.is_empty(), "expected to find alpha_function");
        assert!(results[0].file.contains("alpha"));
    }

    #[tokio::test]
    async fn index_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut binary = vec![0u8; 100];
        binary[0] = 0;
        std::fs::write(dir.path().join("bin.dat"), &binary).unwrap();
        std::fs::write(dir.path().join("text.txt"), "searchable content here").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);
        idx.spawn_reindex();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let stats = idx.stats().await;
        assert_eq!(stats.files, 1, "binary file should be skipped");
    }

    #[tokio::test]
    async fn search_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);
        let results = idx.search("anything", 10).await;
        assert!(results.is_empty());
    }

    // --- Incremental index tests ---

    async fn wait_for_index() {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    #[tokio::test]
    async fn incremental_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stable.rs"), "fn stable() {}").unwrap();
        std::fs::write(dir.path().join("other.rs"), "fn other() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);

        idx.spawn_reindex();
        wait_for_index().await;
        let stats1 = idx.stats().await;
        assert_eq!(stats1.files, 2);

        // Reindex without changes — should still have 2 files
        idx.spawn_reindex();
        wait_for_index().await;
        let stats2 = idx.stats().await;
        assert_eq!(stats2.files, 2);

        // Search still works
        let results = idx.search("stable", 10).await;
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn incremental_adds_new_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.rs"), "fn existing() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 1);

        // Add a new file
        std::fs::write(
            dir.path().join("brand_new.rs"),
            "fn brand_new_function() {}",
        )
        .unwrap();

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 2);

        let results = idx.search("brand_new_function", 10).await;
        assert!(!results.is_empty(), "new file should be searchable");
    }

    #[tokio::test]
    async fn incremental_removes_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn keep() {}").unwrap();
        std::fs::write(dir.path().join("delete_me.rs"), "fn delete_me_unique() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 2);

        let results = idx.search("delete_me_unique", 10).await;
        assert!(!results.is_empty());

        // Delete the file
        std::fs::remove_file(dir.path().join("delete_me.rs")).unwrap();

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 1);

        let results = idx.search("delete_me_unique", 10).await;
        assert!(results.is_empty(), "deleted file should not be searchable");
    }

    #[tokio::test]
    async fn incremental_updates_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mutable.rs"), "fn old_function_name() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);

        idx.spawn_reindex();
        wait_for_index().await;

        let results = idx.search("old_function_name", 10).await;
        assert!(!results.is_empty());

        // Modify the file (need to change mtime — sleep briefly to ensure different mtime)
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        std::fs::write(dir.path().join("mutable.rs"), "fn new_function_name() {}").unwrap();

        idx.spawn_reindex();
        wait_for_index().await;

        let old_results = idx.search("old_function_name", 10).await;
        assert!(
            old_results.is_empty(),
            "old content should not be searchable"
        );

        let new_results = idx.search("new_function_name", 10).await;
        assert!(!new_results.is_empty(), "new content should be searchable");
    }

    #[tokio::test]
    async fn incremental_combined_add_delete_modify() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn keep_unchanged() {}").unwrap();
        std::fs::write(dir.path().join("modify.rs"), "fn before_modify() {}").unwrap();
        std::fs::write(dir.path().join("remove.rs"), "fn will_be_removed() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg);

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 3);

        // Modify, delete, and add simultaneously
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        std::fs::write(dir.path().join("modify.rs"), "fn after_modify() {}").unwrap();
        std::fs::remove_file(dir.path().join("remove.rs")).unwrap();
        std::fs::write(dir.path().join("added.rs"), "fn freshly_added() {}").unwrap();

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 3); // keep + modify + added

        // Unchanged file still searchable
        assert!(!idx.search("keep_unchanged", 10).await.is_empty());
        // Modified file has new content
        assert!(idx.search("before_modify", 10).await.is_empty());
        assert!(!idx.search("after_modify", 10).await.is_empty());
        // Deleted file gone
        assert!(idx.search("will_be_removed", 10).await.is_empty());
        // New file searchable
        assert!(!idx.search("freshly_added", 10).await.is_empty());
    }
}
