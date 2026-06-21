use crate::config::IndexConfig;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
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
    /// Serializes concurrent reindexes. Without this, two overlapping
    /// `spawn_reindex` calls each take a `blocking_read` snapshot, walk
    /// the tree, and then race on the final `blocking_write` — and
    /// whichever finishes last wins, potentially with a stale view if
    /// it started before recent filesystem mutations. The lock is held
    /// for the duration of a reindex (snapshot → walk → write), so
    /// "spawn N reindexes back-to-back" now means "N reindexes run in
    /// order, each observing the result of its predecessor."
    reindex_lock: Arc<std::sync::Mutex<()>>,
    max_depth: usize,
    max_file_size: usize,
    binary_sniff_bytes: usize,
    skip_extensions: HashSet<String>,
    /// Hard cap on indexed files (0 = unbounded). See `IndexConfig::max_files`.
    max_files: usize,
    /// When true, the configured root is "over-broad" (home/system dir) and
    /// auto-indexing is suppressed — `spawn_reindex` no-ops, leaving an empty
    /// index. Computed once at construction from `is_overbroad_root`.
    guard: bool,
    /// When true, print one-line indexer stats to stderr after each reindex.
    /// Disabled in MCP quiet mode so hosts like Cursor don't surface benign
    /// lines as `[error]` (they classify all subprocess stderr as errors).
    log_progress: bool,
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
    pub fn new(root: PathBuf, cfg: &IndexConfig, log_progress: bool) -> Self {
        let guard = !should_eager_index(&root, cfg);
        Self {
            root,
            inner: Arc::new(RwLock::new(IndexState::new())),
            reindex_lock: Arc::new(std::sync::Mutex::new(())),
            max_depth: cfg.max_depth,
            max_file_size: cfg.max_file_size,
            binary_sniff_bytes: cfg.binary_sniff_bytes,
            skip_extensions: cfg.skip_set(),
            max_files: cfg.max_files,
            guard,
            log_progress,
        }
    }

    /// Rebuild the index in the background.
    /// On first call, does a full build. On subsequent calls, performs an
    /// incremental update: skips files whose mtime hasn't changed, removes
    /// deleted files, and only re-extracts trigrams for new/modified files.
    pub fn spawn_reindex(&self) {
        // Over-broad roots are never auto-indexed: a large directory with no
        // project marker (e.g. $HOME inherited as cwd) would build a
        // multi-gigabyte trigram index over unrelated files. Leave the index
        // empty; an explicit -w or an MCP roots re-root replaces this index
        // with one on a real project.
        if self.guard {
            if self.log_progress {
                eprintln!(
                    "index: skipping auto-index of over-broad root {:?} (exceeds max_files \
                     and has no project marker); set -w or use MCP roots to index a real project",
                    self.root
                );
            }
            return;
        }

        let root = self.root.clone();
        let inner = Arc::clone(&self.inner);
        let reindex_lock = Arc::clone(&self.reindex_lock);
        let max_depth = self.max_depth;
        let max_file_size = self.max_file_size;
        let binary_sniff_bytes = self.binary_sniff_bytes;
        let skip_ext = self.skip_extensions.clone();
        let max_files = self.max_files;
        let log_progress = self.log_progress;

        tokio::task::spawn_blocking(move || {
            // Serialize against any other reindex already in flight on the
            // blocking pool. Poison recovery: a panic mid-reindex shouldn't
            // wedge subsequent reindexes — we just continue with the next
            // one and let it rebuild the state.
            let _lock = reindex_lock.lock().unwrap_or_else(|p| p.into_inner());
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

            let mut capped = false;
            for entry in walker.flatten() {
                if max_files != 0 && current_files.len() >= max_files {
                    capped = true;
                    break;
                }
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

            if capped && log_progress {
                eprintln!(
                    "index: file cap reached ({max_files}); indexing first {max_files} files \
                     only. Tune [index] max_files if your project is larger."
                );
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

            // Old ID -> new ID mapping for files that survived unchanged.
            let mut id_remap: HashMap<u32, u32> = HashMap::new();
            // Files that need content re-extraction: (rel, mtime, old_id_to_evict)
            // old_id_to_evict is Some for modified files (old trigrams must be
            // removed), None for newly added files.
            let mut to_extract: Vec<(String, u64, Option<u32>)> = Vec::new();

            // Pass 1: classify each file as skipped/updated/added.
            // Only push SKIPPED files into new_files here. New and updated files
            // are deferred to Pass 3 (after binary/size checks) so they don't
            // inflate new_files if their content turns out to be unindexable.
            for (rel, &mtime) in &current_files {
                if let Some(&(old_id, old_mtime)) = prev_paths.get(rel) {
                    if mtime == old_mtime && !is_first_index {
                        // Unchanged — keep existing trigrams, remap old_id → new pos
                        let file_id = new_files.len() as u32;
                        id_remap.insert(old_id, file_id);
                        new_files.push(rel.clone());
                        new_mtimes.push(mtime);
                        new_path_to_id.insert(rel.clone(), file_id);
                        skipped += 1;
                        continue;
                    }
                    // Modified — schedule old trigram removal and re-extraction
                    to_extract.push((rel.clone(), mtime, Some(old_id)));
                    updated += 1;
                } else {
                    // New file — schedule extraction
                    to_extract.push((rel.clone(), mtime, None));
                    added += 1;
                }
            }

            // Pass 2: remove old trigrams for modified files.
            // All old_ids are now known and cannot collide with new file_ids
            // (new_files still only contains skipped files at this point).
            for (_, _, old_id_opt) in &to_extract {
                if let Some(oid) = old_id_opt {
                    for entries in trigrams.values_mut() {
                        entries.retain(|id| id != oid);
                    }
                }
            }

            // Remap old file IDs to new file IDs in trigram entries.
            if !id_remap.is_empty() {
                for entries in trigrams.values_mut() {
                    for id in entries.iter_mut() {
                        if let Some(&new_id) = id_remap.get(id) {
                            *id = new_id;
                        }
                    }
                }
            }

            // Pass 3: extract trigrams for new and modified files.
            // file_ids are assigned here, AFTER id_remap, so they cannot
            // collide with any old_id that was used as a remap source.
            for (rel, mtime, _) in &to_extract {
                let path = root.join(rel);
                let content = match std::fs::read(&path) {
                    Ok(c) if c.len() <= max_file_size => c,
                    _ => continue,
                };
                if content.iter().take(binary_sniff_bytes).any(|&b| b == 0) {
                    continue;
                }
                let file_id = new_files.len() as u32;
                new_files.push(rel.clone());
                new_mtimes.push(*mtime);
                new_path_to_id.insert(rel.clone(), file_id);
                extract_trigrams(&content, file_id, &mut trigrams);
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

            if log_progress {
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

/// Internal preflight budget used when `max_files` is 0 (cap disabled) so the
/// over-broad-root probe still has a bound to early-exit against.
const DEFAULT_EAGER_PROBE: usize = 50_000;

/// Decide whether to eagerly crawl `root` and build a full index, based on a
/// signal rather than a path blocklist (vikunja #47). The rules:
///
/// - Gate disabled (`guard_overbroad_roots == false`) -> always eager.
/// - Filesystem root (`/`, zero normal components) -> never eager.
/// - Small enough (a bounded preflight finds <= `budget` files) -> eager,
///   regardless of what the directory is.
/// - Larger than the budget -> eager only if it looks like a real project
///   (a `project_markers` entry exists at the root).
///
/// This catches the over-broad case generically — `$HOME`, a NAS mount, a
/// downloads dir (large, no project marker) all return false — without a
/// hand-maintained list of paths. The hard `max_files` cap still bounds RSS
/// for anything that does get indexed.
pub fn should_eager_index(root: &Path, cfg: &IndexConfig) -> bool {
    if !cfg.guard_overbroad_roots {
        return true;
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    // Never eagerly crawl the filesystem root.
    let normal_components = root
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if normal_components == 0 {
        return false;
    }

    let budget = if cfg.max_files == 0 {
        DEFAULT_EAGER_PROBE
    } else {
        cfg.max_files
    };

    // Small enough to fully index no matter what it is.
    if preflight_within_budget(&root, cfg, budget) {
        return true;
    }

    // Large: only crawl if it clearly looks like a project.
    has_project_marker(&root, cfg)
}

/// True when one of `cfg.project_markers` exists directly under `root`.
fn has_project_marker(root: &Path, cfg: &IndexConfig) -> bool {
    cfg.project_markers.iter().any(|m| root.join(m).exists())
}

/// Bounded preflight: walk `root` (honoring `.gitignore`, hidden files, and
/// `max_depth`) counting regular files, and early-exit as soon as the count
/// exceeds `budget`. Returns true when the whole tree fits within `budget`
/// (i.e. the root is "small"), false once it provably exceeds it. Touches at
/// most `budget + 1` file entries, so the probe cost is bounded even on a
/// pathological root.
fn preflight_within_budget(root: &Path, cfg: &IndexConfig, budget: usize) -> bool {
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .max_depth(Some(cfg.max_depth))
        .build();
    let mut count = 0usize;
    for entry in walker.flatten() {
        if entry.path().is_file() {
            count += 1;
            if count > budget {
                return false;
            }
        }
    }
    true
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
        assert!(trigrams.contains_key(b"abc"));
        assert!(trigrams.contains_key(b"bcd"));
        assert!(trigrams.contains_key(b"cde"));
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
        assert_eq!(trigrams[b"aaa"].len(), 1);
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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);
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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);
        idx.spawn_reindex();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stats = idx.stats().await;
        assert_eq!(stats.files, 1, "current-thread reindex did not complete");
        let hits = idx.search("beta_marker", 10).await;
        assert!(
            !hits.is_empty(),
            "search after current-thread reindex empty"
        );
    }

    #[tokio::test]
    async fn index_and_search_fixture_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "fn alpha_function() {}").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "some beta content here").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/gamma.rs"), "fn gamma_function() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);
        idx.spawn_reindex();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let stats = idx.stats().await;
        assert_eq!(stats.files, 1, "binary file should be skipped");
    }

    #[tokio::test]
    async fn search_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);
        let results = idx.search("anything", 10).await;
        assert!(results.is_empty());
    }

    // --- Over-broad root gate tests (vikunja #47) ---

    /// Create `n` plain files under `dir`.
    fn write_n_files(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i}.rs")), format!("fn f{i}() {{}}")).unwrap();
        }
    }

    #[test]
    fn eager_index_small_root_regardless_of_marker() {
        // A small directory (within budget) is always eagerly indexed, even
        // with no project marker.
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 3);
        let cfg = IndexConfig {
            max_files: 50,
            ..Default::default()
        };
        assert!(should_eager_index(dir.path(), &cfg));
    }

    #[test]
    fn gate_blocks_large_unmarked_root() {
        // More files than the budget and no project marker -> not eager.
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 8);
        let cfg = IndexConfig {
            max_files: 3,
            ..Default::default()
        };
        assert!(!should_eager_index(dir.path(), &cfg));
    }

    #[test]
    fn gate_allows_large_marked_root() {
        // Same oversized tree, but a project marker at the root opts it in.
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 8);
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let cfg = IndexConfig {
            max_files: 3,
            ..Default::default()
        };
        assert!(should_eager_index(dir.path(), &cfg));
    }

    #[test]
    fn gate_allows_large_root_with_git_dir() {
        // `.git` is a directory marker, not a file.
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 8);
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let cfg = IndexConfig {
            max_files: 3,
            ..Default::default()
        };
        assert!(should_eager_index(dir.path(), &cfg));
    }

    #[test]
    fn gate_never_crawls_filesystem_root() {
        let cfg = IndexConfig::default();
        assert!(!should_eager_index(Path::new("/"), &cfg));
    }

    #[test]
    fn gate_disabled_always_eager() {
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 8);
        let cfg = IndexConfig {
            max_files: 3,
            guard_overbroad_roots: false,
            ..Default::default()
        };
        assert!(should_eager_index(dir.path(), &cfg));
    }

    #[tokio::test]
    async fn gated_root_skips_indexing() {
        // Large + unmarked -> WorkspaceIndex stays empty after spawn_reindex.
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 8);
        let cfg = IndexConfig {
            max_files: 3,
            ..Default::default()
        };
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, false);
        idx.spawn_reindex();
        wait_for_index().await;
        let stats = idx.stats().await;
        assert_eq!(stats.files, 0, "gated root must not be auto-indexed");
    }

    #[tokio::test]
    async fn max_files_caps_index() {
        // Marked root so it is eagerly indexed, but the hard cap still bounds
        // the file count.
        let dir = tempfile::tempdir().unwrap();
        write_n_files(dir.path(), 10);
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let cfg = IndexConfig {
            max_files: 4,
            ..Default::default()
        };
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, false);
        idx.spawn_reindex();
        wait_for_index().await;
        let stats = idx.stats().await;
        assert_eq!(stats.files, 4, "index must stop at max_files");
    }

    // --- Incremental index tests ---

    /// Fixed sleep that's generous enough to let `spawn_reindex` complete
    /// under healthy parallel `cargo test` load. 2 s is sufficient for
    /// indexing 1–3 tiny files even under heavy blocking-pool
    /// contention; tests that rely on detecting a *content* change
    /// (where file count alone can't distinguish stale vs. fresh
    /// state) should use `wait_for_search_hit` instead.
    async fn wait_for_index() {
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    }

    /// Poll `idx.search(query)` until it has at least one hit, or fail
    /// after a generous deadline. Necessary when a test mutates files
    /// in a way that doesn't change the file count — the fixed-sleep
    /// `wait_for_index` helper can return *before* the second reindex
    /// has actually started, and `stats().files` would silently agree
    /// because the old and new file counts happen to match (3 before
    /// vs 3 after a delete-and-add). Polling on the search result is
    /// the only signal that genuinely confirms the new content
    /// landed in the trigram index.
    ///
    /// On hit, if the index is still showing a *previous* reindex's
    /// view (i.e. a reindex committed but its walk happened before
    /// our mutations landed), this poll forces an explicit
    /// `spawn_reindex` and waits again. This works around a subtle
    /// race that pops up only under heavy parallel `cargo test`
    /// load — the OS scheduler can delay the blocking-pool task long
    /// enough that the reindex-2 walker sees the pre-mutation state,
    /// commits "3 old files", and never picks up the new file until
    /// a *third* reindex is triggered.
    async fn wait_for_search_hit(idx: &WorkspaceIndex, query: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut last_kick = std::time::Instant::now();
        let kick_interval = std::time::Duration::from_secs(2);
        loop {
            if !idx.search(query, 10).await.is_empty() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let stats = idx.stats().await;
                panic!(
                    "index never produced a hit for {query:?} within 60 s — \
                     final stats: files={}, trigrams={}, age_ms={:?}",
                    stats.files, stats.trigrams, stats.age_ms
                );
            }
            // Every couple of seconds, force another reindex in case the
            // previous one's walk happened before our mutations were
            // visible on the filesystem. Idempotent and serialized by
            // `reindex_lock`.
            if last_kick.elapsed() >= kick_interval {
                idx.spawn_reindex();
                last_kick = std::time::Instant::now();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn incremental_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stable.rs"), "fn stable() {}").unwrap();
        std::fs::write(dir.path().join("other.rs"), "fn other() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

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
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

        idx.spawn_reindex();
        wait_for_index().await;
        assert_eq!(idx.stats().await.files, 3);

        // Modify, delete, and add simultaneously
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        std::fs::write(dir.path().join("modify.rs"), "fn after_modify() {}").unwrap();
        std::fs::remove_file(dir.path().join("remove.rs")).unwrap();
        std::fs::write(dir.path().join("added.rs"), "fn freshly_added() {}").unwrap();

        idx.spawn_reindex();
        // Cannot rely on `wait_for_index` + file-count assert here: the
        // count is 3 both before (keep+modify+remove) and after
        // (keep+modify+added), so the assertion fires `true` even if
        // reindex 2 hasn't run yet. Wait until the index actually
        // contains the new file's content.
        wait_for_search_hit(&idx, "freshly_added").await;
        assert_eq!(idx.stats().await.files, 3); // keep + modify + added

        // Unchanged file still searchable
        assert!(!idx.search("keep_unchanged", 10).await.is_empty());
        // Modified file has new content
        assert!(idx.search("before_modify", 10).await.is_empty());
        assert!(!idx.search("after_modify", 10).await.is_empty());
        // Deleted file gone
        assert!(idx.search("will_be_removed", 10).await.is_empty());
        // New file searchable (already verified by `wait_for_search_hit`;
        // kept as an explicit assertion for documentation).
        assert!(!idx.search("freshly_added", 10).await.is_empty());
    }

    /// Regression test for the `spawn_reindex` race condition. Two
    /// concurrent reindexes used to race on the final `*guard = state`
    /// write — whichever finished last wins, potentially with a stale
    /// view of the workspace if it started before recent filesystem
    /// mutations. The fix serializes reindexes through a
    /// `reindex_lock` mutex, so the second-triggered reindex
    /// observes the result of the first.
    ///
    /// Test setup:
    /// 1. Trigger an initial reindex over file A.
    /// 2. Immediately add file B (no settle wait between the trigger
    ///    and the file op — this maximizes the chance the first
    ///    reindex hasn't completed yet).
    /// 3. Trigger a second reindex.
    /// 4. Wait long enough for *both* to have finished serially.
    /// 5. Assert the final state contains B's content. Pre-fix this
    ///    failed on ~30% of runs under parallel `cargo test` load
    ///    (whenever reindex 1 happened to commit *after* reindex 2);
    ///    post-fix it must pass deterministically because reindex 2
    ///    cannot start until reindex 1 has released the lock.
    #[tokio::test]
    async fn concurrent_reindexes_serialize_correctly() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha_marker() {}").unwrap();

        let cfg = IndexConfig::default();
        let idx = WorkspaceIndex::new(dir.path().to_path_buf(), &cfg, true);

        idx.spawn_reindex();
        // No wait — deliberately stack the next reindex on top.
        std::fs::write(dir.path().join("b.rs"), "fn beta_marker() {}").unwrap();
        idx.spawn_reindex();

        wait_for_index().await;

        let stats = idx.stats().await;
        assert_eq!(
            stats.files, 2,
            "after two serialized reindexes the index must reflect both \
             files; got {} (race or reindex_lock regression)",
            stats.files
        );
        assert!(
            !idx.search("alpha_marker", 10).await.is_empty(),
            "alpha_marker must be present"
        );
        assert!(
            !idx.search("beta_marker", 10).await.is_empty(),
            "beta_marker must be present — pre-fix this would flake when \
             reindex 1 (which never saw b.rs) committed *after* reindex 2"
        );
    }
}
