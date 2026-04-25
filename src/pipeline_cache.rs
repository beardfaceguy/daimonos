use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Caches tool command results keyed on workspace content hash.
/// Automatically invalidates when source files change (via inotify).
pub struct PipelineCache {
    inner: Arc<RwLock<CacheState>>,
    _watcher: Option<RecommendedWatcher>,
}

struct CacheState {
    /// (tool_id, command) -> CachedResult
    entries: HashMap<(String, String), CachedResult>,
    /// Tracks whether any source file has changed since last cache
    dirty: bool,
    /// When the last invalidation happened
    last_invalidated: Instant,
}

struct CachedResult {
    output: serde_json::Value,
    exit_code: i32,
    #[allow(dead_code)] // retained for future TTL-based eviction
    created: Instant,
}

impl PipelineCache {
    pub fn new(watch_path: &Path) -> Self {
        let inner = Arc::new(RwLock::new(CacheState {
            entries: HashMap::new(),
            dirty: false,
            last_invalidated: Instant::now(),
        }));

        let inner_clone = inner.clone();
        let watcher = Self::start_watcher(watch_path, inner_clone);

        Self {
            inner,
            _watcher: watcher,
        }
    }

    fn start_watcher(
        path: &Path,
        inner: Arc<RwLock<CacheState>>,
    ) -> Option<RecommendedWatcher> {
        let inner_clone = inner.clone();
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            if let Ok(event) = res {
                if event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove() {
                    let inner = inner_clone.clone();
                    // Mark dirty -- invalidation happens on next cache check
                    // Using std::thread since this is a sync callback
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Handle::current();
                        rt.block_on(async {
                            let mut state = inner.write().await;
                            state.dirty = true;
                        });
                    });
                }
            }
        })
        .ok()?;

        watcher.watch(path, RecursiveMode::Recursive).ok()?;
        Some(watcher)
    }

    /// Get a cached result for a tool command, or None if cache miss or dirty.
    pub async fn get(&self, tool_id: &str, command: &str) -> Option<(serde_json::Value, i32)> {
        let mut state = self.inner.write().await;

        if state.dirty {
            state.entries.clear();
            state.dirty = false;
            state.last_invalidated = Instant::now();
            return None;
        }

        let key = (tool_id.to_string(), command.to_string());
        state.entries.get(&key).map(|c| (c.output.clone(), c.exit_code))
    }

    /// Store a result in the cache.
    pub async fn put(
        &self,
        tool_id: &str,
        command: &str,
        output: serde_json::Value,
        exit_code: i32,
    ) {
        let mut state = self.inner.write().await;
        let key = (tool_id.to_string(), command.to_string());
        state.entries.insert(
            key,
            CachedResult {
                output,
                exit_code,
                created: Instant::now(),
            },
        );
    }

    pub async fn stats(&self) -> serde_json::Value {
        let state = self.inner.read().await;
        serde_json::json!({
            "entries": state.entries.len(),
            "dirty": state.dirty,
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

    fn temp_cache() -> (tempfile::TempDir, PipelineCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = PipelineCache::new(dir.path());
        (dir, cache)
    }

    #[tokio::test]
    async fn cache_miss_on_empty() {
        let (_dir, cache) = temp_cache();
        assert!(cache.get("tool1", "build").await.is_none());
    }

    #[tokio::test]
    async fn put_then_get_returns_cached() {
        let (_dir, cache) = temp_cache();
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
        let (_dir, cache) = temp_cache();
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
        let (_dir, cache) = temp_cache();
        cache.put("tool1", "build", json!({"ok": true}), 0).await;

        {
            let mut state = cache.inner.write().await;
            state.dirty = true;
        }

        assert!(cache.get("tool1", "build").await.is_none());
        // dirty flag is reset after clear
        let state = cache.inner.read().await;
        assert!(!state.dirty);
        assert!(state.entries.is_empty());
    }

    #[tokio::test]
    async fn stats_reports_entry_count() {
        let (_dir, cache) = temp_cache();
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
}
