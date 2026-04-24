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
