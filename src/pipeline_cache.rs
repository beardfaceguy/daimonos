use crate::config::PipelineCacheConfig;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Directory base names that are never worth watching for cache invalidation.
/// These are skipped even when `.gitignore` doesn't exclude them and even when
/// they aren't hidden (e.g. `target`, `node_modules`). Without this filter a
/// recursive watcher on a typical project tree can register tens of thousands
/// of inotify watches and exhaust `fs.inotify.max_user_watches`.
const BUILTIN_SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    ".next",
    ".nuxt",
    ".turbo",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
];

/// Caches tool command results keyed on workspace content hash.
/// Automatically invalidates when source files change (via inotify/FSEvents).
#[allow(dead_code)] // wired into Session but methods called only from test + future pipeline paths
pub struct PipelineCache {
    inner: Arc<RwLock<CacheState>>,
    dirty_flag: Arc<AtomicBool>,
    _watcher: Option<RecommendedWatcher>,
}

#[allow(dead_code)]
struct CacheState {
    /// (tool_id, command) -> CachedResult
    entries: HashMap<(String, String), CachedResult>,
    /// When the last invalidation happened
    last_invalidated: Instant,
    /// Per-instance cap on `entries.len()`, taken from
    /// `PipelineCacheConfig.max_entries` at construction. Stored on the
    /// state (rather than on `PipelineCache`) so eviction logic can read
    /// it under the same write lock.
    max_entries: usize,
    /// Strictly-monotonic logical clock incremented on every `get`/`put`
    /// hit. Used to drive LRU eviction without relying on wall-clock
    /// timestamps (which can tie at sub-millisecond resolution and
    /// require sleeps in tests). The counter is bumped on the same write
    /// lock that mutates the entry it's stamping, so the value assigned
    /// to `CachedResult.last_touched` is always unique.
    access_counter: u64,
}

#[allow(dead_code)]
struct CachedResult {
    output: serde_json::Value,
    exit_code: i32,
    /// Value of `CacheState.access_counter` at the most recent touch
    /// (insert or read). Eviction picks the entry with the smallest
    /// `last_touched` — i.e. the least-recently-used entry.
    last_touched: u64,
}

#[allow(dead_code)]
impl PipelineCache {
    /// Construct a pipeline cache using the built-in default watcher config.
    /// Production code should prefer [`PipelineCache::with_config`] so that
    /// operators can tune the watch cap and ignore list.
    pub fn new(watch_path: &Path) -> Self {
        Self::with_config(watch_path, &PipelineCacheConfig::default())
    }

    pub fn with_config(watch_path: &Path, cfg: &PipelineCacheConfig) -> Self {
        let inner = Arc::new(RwLock::new(CacheState {
            entries: HashMap::new(),
            last_invalidated: Instant::now(),
            max_entries: cfg.max_entries,
            access_counter: 0,
        }));

        let dirty_flag = Arc::new(AtomicBool::new(false));
        let watcher = Self::start_watcher(watch_path, dirty_flag.clone(), cfg);

        Self {
            inner,
            dirty_flag,
            _watcher: watcher,
        }
    }

    /// Build a recursive-equivalent watcher by walking the workspace once with
    /// the `ignore` crate (respecting `.gitignore`, hidden files, and a
    /// built-in skip list of heavyweight build/dependency dirs) and
    /// registering one **non-recursive** watch per surviving directory.
    ///
    /// This avoids `RecursiveMode::Recursive`'s pathological behaviour on
    /// Linux where every subdirectory of `node_modules`/`target`/`.git`
    /// burns an inotify watch. Total watches are hard-capped by
    /// `cfg.max_watches`; on overflow we log and stop adding watches —
    /// changes in unwatched dirs simply won't invalidate the cache, which
    /// is preferable to exhausting `fs.inotify.max_user_watches`.
    fn start_watcher(
        path: &Path,
        dirty: Arc<AtomicBool>,
        cfg: &PipelineCacheConfig,
    ) -> Option<RecommendedWatcher> {
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            if let Ok(event) = res {
                if event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove() {
                    dirty.store(true, Ordering::Relaxed);
                }
            }
        })
        .ok()?;

        let max_watches = cfg.max_watches.max(1);
        let extra_skip: HashSet<String> = cfg.extra_ignore_dirs.iter().cloned().collect();

        let walker = ignore::WalkBuilder::new(path)
            .hidden(true)
            .git_ignore(true)
            .filter_entry(move |entry| {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    // Files don't get their own watch; only dirs reach the
                    // watch loop below. Keep them in the walk so the parent
                    // dir is still visited, but cheaply skip the filter check.
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                if BUILTIN_SKIP_DIRS.iter().any(|d| *d == name.as_ref()) {
                    return false;
                }
                if extra_skip.contains(name.as_ref()) {
                    return false;
                }
                true
            })
            .build();

        let mut watched: usize = 0;
        let mut overflow = false;
        let mut errors: usize = 0;

        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if watched >= max_watches {
                overflow = true;
                break;
            }
            match watcher.watch(entry.path(), RecursiveMode::NonRecursive) {
                Ok(()) => watched += 1,
                Err(_) => errors += 1,
            }
        }

        if overflow {
            eprintln!(
                "pipeline_cache: watch cap reached ({max_watches} dirs); further changes \
                 outside watched dirs will not invalidate the cache. Tune \
                 [pipeline_cache] max_watches or extra_ignore_dirs in daimonos.toml."
            );
        }
        if errors > 0 {
            eprintln!("pipeline_cache: {errors} dir(s) failed to register a watch");
        }

        if watched == 0 {
            // Nothing watched and no successful registrations — drop the
            // watcher so we don't keep a useless inotify fd around.
            return None;
        }

        Some(watcher)
    }

    /// Get a cached result for a tool command, or None if cache miss or dirty.
    ///
    /// A hit bumps the entry's `last_touched` stamp so that the next
    /// eviction protects recently-used entries. This requires a write
    /// lock; the cache is small and write-heavy (every tool invocation
    /// touches it) so the extra contention is preferable to a
    /// stale-LRU-stamp footgun.
    pub async fn get(&self, tool_id: &str, command: &str) -> Option<(serde_json::Value, i32)> {
        if self.dirty_flag.swap(false, Ordering::Relaxed) {
            let mut state = self.inner.write().await;
            state.entries.clear();
            state.last_invalidated = Instant::now();
            return None;
        }

        let mut state = self.inner.write().await;
        let key = (tool_id.to_string(), command.to_string());
        // Compute the next stamp first (it's a u64 copy) then assign it
        // only on a hit. Bumping the counter on misses is still correct
        // (it just leaves gaps in the logical clock) but wastes values.
        let next = state.access_counter.wrapping_add(1);
        let result = state.entries.get_mut(&key).map(|c| {
            c.last_touched = next;
            (c.output.clone(), c.exit_code)
        });
        if result.is_some() {
            state.access_counter = next;
        }
        result
    }

    /// Store a result in the cache. When the entry cap is reached, evicts
    /// the least-recently-used entry (the one with the smallest
    /// `last_touched` stamp). Inserting a fresh entry — or overwriting an
    /// existing one — counts as a recency touch.
    pub async fn put(
        &self,
        tool_id: &str,
        command: &str,
        output: serde_json::Value,
        exit_code: i32,
    ) {
        let mut state = self.inner.write().await;
        let key = (tool_id.to_string(), command.to_string());

        // Insert-or-overwrite path doesn't need eviction; only growth past
        // the cap does. Checking key presence first avoids evicting an
        // entry we're about to overwrite.
        if !state.entries.contains_key(&key) && state.entries.len() >= state.max_entries {
            let evict_key = state
                .entries
                .iter()
                .min_by_key(|(_, v)| v.last_touched)
                .map(|(k, _)| k.clone());
            if let Some(k) = evict_key {
                state.entries.remove(&k);
            }
        }

        let next = state.access_counter.wrapping_add(1);
        state.access_counter = next;
        state.entries.insert(
            key,
            CachedResult {
                output,
                exit_code,
                last_touched: next,
            },
        );
    }

    pub async fn stats(&self) -> serde_json::Value {
        let state = self.inner.read().await;
        let dirty = self.dirty_flag.load(Ordering::Relaxed);
        serde_json::json!({
            "entries": state.entries.len(),
            "dirty": dirty,
            "last_invalidated_ms": state.last_invalidated.elapsed().as_millis() as u64,
        })
    }

    /// Compute a content hash for a file's bytes.
    pub fn hash_file(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hex::encode(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    /// Lock guard alias: zero-sized struct on non-Linux so the
    /// `let (_dir, _cache, _lock) = temp_cache();` destructure stays
    /// portable. On Linux the third field is the real
    /// `INOTIFY_TEST_LOCK` guard held for the test body.
    #[cfg(target_os = "linux")]
    type InotifyTestGuard = std::sync::MutexGuard<'static, ()>;
    #[cfg(not(target_os = "linux"))]
    type InotifyTestGuard = ();

    /// Note: the returned guard is `std::sync::MutexGuard<'static, ()>`
    /// on Linux, which is `!Send`. Tests using `temp_cache()` are
    /// `#[tokio::test]` (current-thread runtime by default), so
    /// holding it across `.await` is fine — but if any test is later
    /// annotated `flavor = "multi_thread"`, the compiler will reject
    /// it. At that point, drop the guard explicitly before the first
    /// `.await` or switch to a `tokio::sync::Mutex`.
    fn temp_cache() -> (tempfile::TempDir, PipelineCache, InotifyTestGuard) {
        #[cfg(target_os = "linux")]
        let lock = inotify_test_lock();
        #[cfg(not(target_os = "linux"))]
        let lock: InotifyTestGuard = ();

        let dir = tempfile::tempdir().unwrap();
        let cache = PipelineCache::new(dir.path());
        (dir, cache, lock)
    }

    #[tokio::test]
    async fn cache_miss_on_empty() {
        let (_dir, cache, _lock) = temp_cache();
        assert!(cache.get("tool1", "build").await.is_none());
    }

    #[tokio::test]
    async fn put_then_get_returns_cached() {
        let (_dir, cache, _lock) = temp_cache();
        let output = serde_json::json!({"status": "ok"});
        cache.put("tool1", "build", output.clone(), 0).await;
        let result = cache.get("tool1", "build").await;
        assert!(result.is_some());
        let (val, exit) = result.unwrap();
        assert_eq!(val, output);
        assert_eq!(exit, 0);
    }

    #[tokio::test]
    async fn different_keys_are_independent() {
        let (_dir, cache, _lock) = temp_cache();
        cache.put("tool1", "build", json!({"a": 1}), 0).await;
        cache.put("tool1", "lint", json!({"b": 2}), 1).await;
        cache.put("tool2", "build", json!({"c": 3}), 0).await;

        let r1 = cache.get("tool1", "build").await.unwrap();
        assert_eq!(r1.0, json!({"a": 1}));
        let r2 = cache.get("tool1", "lint").await.unwrap();
        assert_eq!(r2.1, 1);
        let r3 = cache.get("tool2", "build").await.unwrap();
        assert_eq!(r3.0, json!({"c": 3}));
    }

    #[tokio::test]
    async fn dirty_flag_clears_cache() {
        let (_dir, cache, _lock) = temp_cache();
        cache.put("tool1", "build", json!({"ok": true}), 0).await;

        cache.dirty_flag.store(true, Ordering::Relaxed);

        assert!(cache.get("tool1", "build").await.is_none());
        assert!(!cache.dirty_flag.load(Ordering::Relaxed));
        let state = cache.inner.read().await;
        assert!(state.entries.is_empty());
    }

    #[tokio::test]
    async fn stats_reports_entry_count() {
        let (_dir, cache, _lock) = temp_cache();
        let s1 = cache.stats().await;
        assert_eq!(s1["entries"], 0);
        assert_eq!(s1["dirty"], false);

        cache.put("t", "c", json!(null), 0).await;
        let s2 = cache.stats().await;
        assert_eq!(s2["entries"], 1);
    }

    #[test]
    fn hash_file_deterministic() {
        let h1 = PipelineCache::hash_file(b"hello world");
        let h2 = PipelineCache::hash_file(b"hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn hash_file_different_content() {
        let h1 = PipelineCache::hash_file(b"aaa");
        let h2 = PipelineCache::hash_file(b"bbb");
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn cache_bounded_after_many_entries() {
        let (_dir, cache, _lock) = temp_cache();
        for i in 0..2000 {
            cache
                .put(
                    &format!("tool_{i}"),
                    &format!("cmd_{i}"),
                    json!({"i": i}),
                    0,
                )
                .await;
        }
        let state = cache.inner.read().await;
        assert!(
            state.entries.len() <= PipelineCacheConfig::default().max_entries,
            "pipeline cache should be bounded to the default max_entries cap, but has {} entries",
            state.entries.len()
        );
    }

    /// Regression for vikunja #256: the entry cap must come from
    /// `PipelineCacheConfig.max_entries`, not a hardcoded `const`. Construct
    /// the cache with a tiny custom cap and verify `put()` honors it.
    #[tokio::test]
    async fn cache_max_entries_is_configurable() {
        #[cfg(target_os = "linux")]
        let _lock = inotify_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let cfg = PipelineCacheConfig {
            max_watches: 8192,
            extra_ignore_dirs: Vec::new(),
            max_entries: 4,
        };
        let cache = PipelineCache::with_config(dir.path(), &cfg);

        for i in 0..20 {
            cache
                .put(
                    &format!("tool_{i}"),
                    &format!("cmd_{i}"),
                    json!({"i": i}),
                    0,
                )
                .await;
        }

        let state = cache.inner.read().await;
        assert!(
            state.entries.len() <= cfg.max_entries,
            "configured cap of {} not honored; got {} entries",
            cfg.max_entries,
            state.entries.len()
        );
    }

    /// `PipelineCacheConfig::default()` must still cap at 1024 entries so
    /// the previous hardcoded behavior is preserved when callers don't
    /// override the field.
    #[test]
    fn default_max_entries_preserves_legacy_cap() {
        assert_eq!(
            PipelineCacheConfig::default().max_entries,
            1024,
            "default cap must remain 1024 to match the prior hardcoded MAX_CACHE_ENTRIES"
        );
    }

    /// Regression for vikunja #255. Eviction used to pick
    /// `entries.keys().next()`, an arbitrary key in HashMap iteration order.
    /// Under true LRU, accessing an entry must protect it from the next
    /// eviction; only the least-recently-touched entry should be dropped.
    #[tokio::test]
    async fn cache_evicts_least_recently_used_entry() {
        #[cfg(target_os = "linux")]
        let _lock = inotify_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let cfg = PipelineCacheConfig {
            max_watches: 8192,
            extra_ignore_dirs: Vec::new(),
            max_entries: 3,
        };
        let cache = PipelineCache::with_config(dir.path(), &cfg);

        cache.put("t", "a", json!("a"), 0).await;
        cache.put("t", "b", json!("b"), 0).await;
        cache.put("t", "c", json!("c"), 0).await;

        // Touch "a" so "b" becomes the LRU entry.
        assert!(cache.get("t", "a").await.is_some());

        // This put forces an eviction. With true LRU it must drop "b",
        // not "a". The old `HashMap::keys().next()` implementation would
        // drop an arbitrary entry — passing this test under the old code
        // was a coin flip on the hash seed.
        cache.put("t", "d", json!("d"), 0).await;

        assert!(
            cache.get("t", "a").await.is_some(),
            "recently-accessed 'a' must survive LRU eviction"
        );
        assert!(
            cache.get("t", "b").await.is_none(),
            "least-recently-used 'b' must be evicted"
        );
        assert!(cache.get("t", "c").await.is_some(), "'c' must survive");
        assert!(
            cache.get("t", "d").await.is_some(),
            "just-inserted 'd' must be present"
        );

        let state = cache.inner.read().await;
        assert_eq!(state.entries.len(), 3, "cache size must remain at the cap");
    }

    /// LRU bookkeeping for repeated access: putting the same key multiple
    /// times must update its recency too, not just on `get()`.
    #[tokio::test]
    async fn cache_put_refreshes_recency() {
        #[cfg(target_os = "linux")]
        let _lock = inotify_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let cfg = PipelineCacheConfig {
            max_watches: 8192,
            extra_ignore_dirs: Vec::new(),
            max_entries: 3,
        };
        let cache = PipelineCache::with_config(dir.path(), &cfg);

        cache.put("t", "a", json!("a"), 0).await;
        cache.put("t", "b", json!("b"), 0).await;
        cache.put("t", "c", json!("c"), 0).await;

        // Re-put "a" — its recency should jump to the most recent slot,
        // making "b" the LRU.
        cache.put("t", "a", json!("a_v2"), 0).await;

        cache.put("t", "d", json!("d"), 0).await;

        assert!(
            cache.get("t", "a").await.is_some(),
            "re-put 'a' must survive — put() must refresh recency"
        );
        assert!(
            cache.get("t", "b").await.is_none(),
            "'b' must be evicted as the new LRU"
        );
    }

    /// Process-global lock taken by every test that constructs a
    /// `PipelineCache`. The cache spawns a `notify::Watcher`
    /// background thread that holds inotify watches for the cache's
    /// entire lifetime, so any test with a live cache contributes
    /// `/proc/self/fdinfo` entries that the inotify-counting tests
    /// could see in their measurement window.
    ///
    /// Without this serialization, parallel `cargo test` runs can
    /// have one inotify-counting test capture `baseline =
    /// count_inotify_watches()` while another test's PipelineCache
    /// is concurrently active, inflating "added X watches" past
    /// configured caps. Confirmed locally: 1-in-4 flake rate against
    /// `pipeline_cache_respects_max_watches_cap` pre-fix.
    ///
    /// Direction of the remaining (harmless) race after this fix:
    /// "concurrent unlocked watchers inflating the count" is the
    /// behavior we lock out; "previous-test cleanup landing during
    /// `baseline`" is benign because such watches are subtracted out
    /// of the `added` delta.
    ///
    /// Plain `std::sync::Mutex` rather than pulling in `serial_test`
    /// — AGENTS.md asks new crate additions be flagged for
    /// confirmation, and a static mutex is the smaller hammer.
    #[cfg(target_os = "linux")]
    static INOTIFY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Centralized acquire for `INOTIFY_TEST_LOCK` with consistent
    /// poison recovery. Every test in this module that constructs a
    /// `PipelineCache` should hold the guard for the test body.
    ///
    /// Poison recovery (`unwrap_or_else(|p| p.into_inner())`) keeps
    /// one panicking test from cascading through the rest. Note that
    /// a panicking test still drops its `PipelineCache` on unwind
    /// (Rust's stack unwinding runs `Drop` impls), so its inotify
    /// watches are released to the kernel before any subsequent
    /// test observes `count_inotify_watches()`.
    ///
    /// On non-Linux platforms `count_inotify_watches()` always
    /// returns 0 and the inotify-counting tests are `#[cfg(target_os
    /// = "linux")]`, so the lock is also Linux-only.
    #[cfg(target_os = "linux")]
    fn inotify_test_lock() -> std::sync::MutexGuard<'static, ()> {
        INOTIFY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Counts the number of active inotify watch descriptors held by the
    /// current process by parsing `/proc/self/fdinfo/*`. Each `inotify wd:`
    /// line corresponds to one kernel watch.
    #[cfg(target_os = "linux")]
    fn count_inotify_watches() -> usize {
        let mut total = 0;
        let dir = match std::fs::read_dir("/proc/self/fdinfo") {
            Ok(d) => d,
            Err(_) => return 0,
        };
        for entry in dir.flatten() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                total += content
                    .lines()
                    .filter(|l| l.starts_with("inotify wd:"))
                    .count();
            }
        }
        total
    }

    /// Regression test for the inotify-watch leak bug.
    ///
    /// `RecursiveMode::Recursive` on Linux registers one inotify watch per
    /// directory in the tree. When the workspace contains heavy vendored
    /// trees (`.git`, `node_modules`, `target`, etc.) this can exhaust
    /// `fs.inotify.max_user_watches`. The watcher must filter ignored
    /// directories before walking, so the watch count tracks the actual
    /// source tree size — not the worst-case directory count.
    ///
    /// `count_inotify_watches()` queries `/proc/self/fdinfo/*` —
    /// process-global state shared with the other two inotify-watch
    /// tests in this module. Without serialization, baselines
    /// captured at the start of one test race against watch creation
    /// in another, producing spurious "added X watches" inflation
    /// and intermittent failures. We hold `INOTIFY_TEST_LOCK` for
    /// the entire body to force these tests to take turns.
    #[cfg(target_os = "linux")]
    #[test]
    fn pipeline_cache_does_not_watch_ignored_dirs() {
        let _lock = inotify_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("tests/it.rs"), "").unwrap();

        for i in 0..200 {
            std::fs::create_dir_all(root.join(format!(".git/objects/{i:02x}"))).unwrap();
            std::fs::create_dir_all(root.join(format!("node_modules/pkg_{i}/dist"))).unwrap();
            std::fs::create_dir_all(root.join(format!("target/debug/build/crate_{i}/out")))
                .unwrap();
        }

        let baseline = count_inotify_watches();
        let cache = PipelineCache::new(root);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let with_cache = count_inotify_watches();
        let added = with_cache.saturating_sub(baseline);
        drop(cache);

        assert!(
            added < 50,
            "PipelineCache added {added} inotify watches for a workspace whose tree is mostly \
             ignored dirs (.git, node_modules, target). Expected < 50: the watcher must skip \
             ignored directories so it doesn't exhaust fs.inotify.max_user_watches."
        );
    }

    /// `max_watches` must be a hard cap: even on a tree of unfiltered source
    /// dirs, the watcher should never register more than the configured
    /// number of inotify watches.
    ///
    /// Holds `INOTIFY_TEST_LOCK` — see
    /// `pipeline_cache_does_not_watch_ignored_dirs` for the rationale.
    /// This test was the original flake source: concurrent watch
    /// creation in the sibling tests inflated `count_inotify_watches()`
    /// past the 25-watch cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn pipeline_cache_respects_max_watches_cap() {
        let _lock = inotify_test_lock();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        for i in 0..400 {
            std::fs::create_dir_all(root.join(format!("src/mod_{i}/sub"))).unwrap();
        }

        let baseline = count_inotify_watches();
        let cfg = PipelineCacheConfig {
            max_watches: 25,
            extra_ignore_dirs: Vec::new(),
            max_entries: 1024,
        };
        let cache = PipelineCache::with_config(root, &cfg);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let added = count_inotify_watches().saturating_sub(baseline);
        drop(cache);

        assert!(
            added <= cfg.max_watches,
            "PipelineCache added {added} inotify watches, exceeding configured cap of {}",
            cfg.max_watches
        );
    }

    /// Watcher must fully release inotify watches when the cache is dropped,
    /// so that short-lived sessions don't accumulate kernel resources.
    ///
    /// Holds `INOTIFY_TEST_LOCK` — see
    /// `pipeline_cache_does_not_watch_ignored_dirs` for the rationale.
    #[cfg(target_os = "linux")]
    #[test]
    fn pipeline_cache_releases_watches_on_drop() {
        let _lock = inotify_test_lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        let baseline = count_inotify_watches();
        {
            let _cache = PipelineCache::new(dir.path());
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        let after = count_inotify_watches();

        let leaked = after.saturating_sub(baseline);
        assert!(
            leaked <= 1,
            "PipelineCache leaked {leaked} inotify watches after Drop (baseline {baseline}, after {after})"
        );
    }
}
