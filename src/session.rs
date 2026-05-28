use crate::analytics::AnalyticsStore;
use crate::config::Config;
use crate::index::WorkspaceIndex;
use crate::pipeline_cache::PipelineCache;
use crate::protocol::ResponseMeta;
use crate::snapshot::SnapshotStore;
use crate::tool_runner::ToolRegistry;
use crate::tools;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Process-global counter for background-process IDs. Shared across all
/// `Session` instances in the same daemon process so concurrent sessions
/// (and concurrent tests) never collide on the same pid — which used to
/// cause race conditions on the shared `/tmp/daimonos_bg_<pid>.log`
/// filename, since every session's `next_pid` started at 1.
static NEXT_BG_PID: AtomicU32 = AtomicU32::new(1);

/// Tracks a file's content hash so re-reads of unchanged files can return a compact response.
#[derive(Debug, Clone)]
pub struct ReadCacheEntry {
    pub hash: u64,
    #[allow(dead_code)] // exposed for diagnostics and future compact re-read responses
    pub lines: usize,
}

/// Per-connection session state.
/// Persists working directory, env vars, and background processes across calls.
pub struct Session {
    pub workspace: PathBuf,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub bg_processes: HashMap<u32, BgProcess>,
    pub index: Option<Arc<WorkspaceIndex>>,
    pub tool_registry: Option<Arc<ToolRegistry>>,
    pub pipeline_cache: Option<Arc<PipelineCache>>,
    pub snapshot_store: SnapshotStore,
    pub cfg: Arc<Config>,
    pub exec_usage: HashMap<String, u32>,
    /// Tracks content hashes of files the model has already read, keyed by canonical path.
    pub read_cache: HashMap<PathBuf, ReadCacheEntry>,
    /// Tools currently exposed in list_tools. Core tools are always present;
    /// extended tools are added on first use or via `list_all_tools`.
    pub exposed_tools: HashSet<String>,
    /// Tools the model has already called this session. Used to strip schemas
    /// from list_tools responses — the model already has them in context.
    pub used_tools: HashSet<String>,
    pub analytics: Option<Arc<AnalyticsStore>>,
    /// Optional caller-supplied identifier for the agent-runtime session
    /// driving this connection (e.g. `claude --session-id <uuid>`).
    /// Bootstrapped from `DAIMONOS_AGENT_SESSION_ID` at startup, can be
    /// updated mid-session via the `set_external_session_id` MCP tool.
    /// Threaded onto every `ToolCallRecord` so the analytics DB can be
    /// joined post-hoc with the agent's own usage logs (vikunja #43).
    pub external_session_id: Option<String>,
    /// Out-of-band metadata produced by the most recent `ops::dispatch` call.
    /// Populated by the MCP layer right before it converts the `Response`
    /// into a `CallToolResult` and consumed by the analytics layer in the
    /// same dispatch turn. Replaces brittle substring matching on the
    /// serialized response text.
    pub last_response_meta: ResponseMeta,
}

pub struct BgProcess {
    pub child: tokio::process::Child,
    pub output_path: PathBuf,
}

impl Session {
    pub fn new(workspace: PathBuf, cfg: Arc<Config>) -> Self {
        let cwd = workspace.clone();
        let env = Self::build_initial_env(&cfg);
        let snapshot_store = SnapshotStore::new(workspace.clone());
        Self {
            workspace,
            cwd,
            env,
            bg_processes: HashMap::new(),
            index: None,
            tool_registry: None,
            pipeline_cache: None,
            snapshot_store,
            cfg,
            exec_usage: HashMap::new(),
            read_cache: HashMap::new(),
            exposed_tools: tools::initial_exposed_tools(),
            used_tools: HashSet::new(),
            analytics: None,
            external_session_id: None,
            last_response_meta: ResponseMeta::default(),
        }
    }

    /// Build the initial session environment with an enhanced PATH.
    /// Combines: config extra_path + auto-detected tool dirs + parent process PATH.
    fn build_initial_env(cfg: &Config) -> HashMap<String, String> {
        let mut env = HashMap::new();

        let parent_path = std::env::var("PATH").unwrap_or_default();
        let parent_dirs: std::collections::HashSet<&str> = parent_path.split(':').collect();

        let mut extra: Vec<String> = Vec::new();

        // Config-specified extra paths (highest priority)
        for dir in &cfg.process.extra_path {
            let expanded = shellexpand_home(dir);
            if std::path::Path::new(&expanded).is_dir() && !parent_dirs.contains(expanded.as_str())
            {
                extra.push(expanded);
            }
        }

        collect_tool_dirs(&parent_dirs, &mut extra);

        let path_val = if !extra.is_empty() {
            let mut new_path = extra.join(":");
            if !parent_path.is_empty() {
                new_path.push(':');
                new_path.push_str(&parent_path);
            }
            new_path
        } else {
            parent_path
        };
        if !path_val.is_empty() {
            env.insert("PATH".to_string(), path_val);
        }

        env
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    pub fn alloc_pid(&mut self) -> u32 {
        // Process-global to avoid collisions on the shared
        // `/tmp/daimonos_bg_<pid>.log` filename when multiple sessions
        // (or multiple parallel tests) live in the same process.
        // `Ordering::Relaxed` is sufficient — we don't need a memory
        // fence, just a unique monotonic value.
        NEXT_BG_PID.fetch_add(1, Ordering::Relaxed)
    }

    /// Hash file content using a fast non-cryptographic hash.
    pub fn content_hash(content: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    /// Check if a file's content matches what the model has already seen.
    /// Returns Some(entry) if the file is cached and unchanged.
    pub fn check_read_cache(&self, path: &PathBuf, content: &str) -> Option<&ReadCacheEntry> {
        let entry = self.read_cache.get(path)?;
        if entry.hash == Self::content_hash(content) {
            Some(entry)
        } else {
            None
        }
    }

    /// Record a file read in the cache. Evicts oldest entries when full.
    pub fn update_read_cache(&mut self, path: PathBuf, content: &str) {
        let max = self.cfg.process.max_cache_entries;
        if self.read_cache.len() >= max {
            let evict_key = self.read_cache.keys().next().cloned();
            if let Some(k) = evict_key {
                self.read_cache.remove(&k);
            }
        }
        let hash = Self::content_hash(content);
        let lines = content.lines().count();
        self.read_cache.insert(path, ReadCacheEntry { hash, lines });
    }

    /// Invalidate the read cache for a path (after write or edit).
    pub fn invalidate_read_cache(&mut self, path: &PathBuf) {
        self.read_cache.remove(path);
    }

    /// Record an exec/bg command invocation, evicting the least-used entry when full.
    pub fn record_exec_usage(&mut self, cmd: String) {
        let max = self.cfg.process.max_cache_entries;
        *self.exec_usage.entry(cmd).or_insert(0) += 1;
        if self.exec_usage.len() > max {
            let evict_key = self
                .exec_usage
                .iter()
                .min_by_key(|(_, v)| *v)
                .map(|(k, _)| k.clone());
            if let Some(k) = evict_key {
                self.exec_usage.remove(&k);
            }
        }
    }

    /// Expose a tool so it appears in future list_tools responses.
    pub fn activate_tool(&mut self, name: &str) {
        self.exposed_tools.insert(name.to_string());
    }

    /// Expose all known tools at once (including on-demand tier 2 tools).
    pub fn activate_all_tools(&mut self) {
        for name in tools::on_demand_names() {
            self.exposed_tools.insert(name.to_string());
        }
    }
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// Collect common tool directories that should be on PATH.
/// Appends to `extra` any existing directories not already in `existing`.
fn collect_tool_dirs(existing: &std::collections::HashSet<&str>, extra: &mut Vec<String>) {
    // System-wide tool directories (Homebrew on macOS, standard Linux paths)
    for dir in &["/opt/homebrew/bin", "/usr/local/bin"] {
        let p = std::path::Path::new(dir);
        if p.is_dir() {
            let s = dir.to_string();
            if !existing.contains(s.as_str()) && !extra.contains(&s) {
                extra.push(s);
            }
        }
    }

    // User-specific tool directories
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let candidates = [
            home.join(".cargo/bin"),
            home.join(".local/bin"),
            home.join("go/bin"),
            home.join(".deno/bin"),
            home.join(".bun/bin"),
            home.join(".local/share/fnm/aliases/default/bin"),
        ];
        for dir in &candidates {
            if dir.is_dir() {
                let s = dir.to_string_lossy().to_string();
                if !existing.contains(s.as_str()) && !extra.contains(&s) {
                    extra.push(s);
                }
            }
        }
    }
}

/// Enhance the process-level PATH with common tool directories.
/// Call once at startup, before plugin `is_available()` checks.
pub fn enhance_process_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    let current_dirs: std::collections::HashSet<&str> = current.split(':').collect();
    let mut extra: Vec<String> = Vec::new();
    collect_tool_dirs(&current_dirs, &mut extra);
    if !extra.is_empty() {
        let mut new_path = extra.join(":");
        if !current.is_empty() {
            new_path.push(':');
            new_path.push_str(&current);
        }
        std::env::set_var("PATH", &new_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(workspace: &str) -> Session {
        Session::new(PathBuf::from(workspace), Arc::new(Config::default()))
    }

    #[test]
    fn resolve_relative_path() {
        let s = test_session("/workspace");
        assert_eq!(
            s.resolve_path("foo.txt"),
            PathBuf::from("/workspace/foo.txt")
        );
        assert_eq!(
            s.resolve_path("sub/bar.rs"),
            PathBuf::from("/workspace/sub/bar.rs")
        );
    }

    #[test]
    fn resolve_absolute_path() {
        let s = test_session("/workspace");
        assert_eq!(s.resolve_path("/tmp/x.txt"), PathBuf::from("/tmp/x.txt"));
    }

    #[test]
    fn alloc_pid_monotonic() {
        let mut s = test_session("/workspace");
        let p1 = s.alloc_pid();
        let p2 = s.alloc_pid();
        let p3 = s.alloc_pid();
        // `NEXT_BG_PID` is process-global, so we don't assert absolute
        // values — other tests in this run may have already advanced it.
        // What matters is monotonicity and contiguity for one session.
        assert_eq!(p2, p1 + 1);
        assert_eq!(p3, p2 + 1);
    }

    #[test]
    fn initial_state() {
        let s = test_session("/workspace");
        assert_eq!(s.workspace, PathBuf::from("/workspace"));
        assert_eq!(s.cwd, PathBuf::from("/workspace"));
        assert!(s.bg_processes.is_empty());
        assert!(s.index.is_none());
    }

    #[test]
    fn path_includes_cargo_bin_if_exists() {
        let s = test_session("/workspace");
        if let Some(home) = std::env::var_os("HOME") {
            let cargo_bin = PathBuf::from(&home).join(".cargo/bin");
            if cargo_bin.is_dir() {
                let path = s.env.get("PATH").expect("PATH should be set");
                assert!(
                    path.contains(&cargo_bin.to_string_lossy().to_string()),
                    "PATH should contain ~/.cargo/bin, got: {path}"
                );
            }
        }
    }

    #[test]
    fn path_inherits_parent_path() {
        let s = test_session("/workspace");
        if let Ok(parent_path) = std::env::var("PATH") {
            if let Some(session_path) = s.env.get("PATH") {
                assert!(
                    session_path.ends_with(&parent_path),
                    "session PATH should end with parent PATH"
                );
            }
        }
    }

    #[test]
    fn extra_path_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let extra = dir.path().join("custom_bin");
        std::fs::create_dir(&extra).unwrap();

        let mut cfg = Config::default();
        cfg.process.extra_path = vec![extra.to_string_lossy().to_string()];
        let s = Session::new(PathBuf::from("/workspace"), Arc::new(cfg));

        let path = s.env.get("PATH").expect("PATH should be set");
        assert!(
            path.contains(&extra.to_string_lossy().to_string()),
            "PATH should contain config extra_path dir"
        );
    }

    #[test]
    fn extra_path_skips_nonexistent_dirs() {
        let mut cfg = Config::default();
        cfg.process.extra_path = vec!["/nonexistent_dir_xyz".into()];
        let s = Session::new(PathBuf::from("/workspace"), Arc::new(cfg));

        if let Some(path) = s.env.get("PATH") {
            assert!(
                !path.contains("/nonexistent_dir_xyz"),
                "PATH should not contain nonexistent dirs"
            );
        }
    }

    #[test]
    fn exposed_tools_includes_tier0_and_tier1() {
        let s = test_session("/workspace");
        let expected = tools::initial_exposed_tools();
        for name in &expected {
            assert!(
                s.exposed_tools.contains(name),
                "tool {name} should be exposed"
            );
        }
    }

    #[test]
    fn on_demand_tools_hidden_by_default() {
        let s = test_session("/workspace");
        for name in tools::on_demand_names() {
            assert!(
                !s.exposed_tools.contains(name),
                "on-demand tool {name} should be hidden until activated"
            );
        }
    }

    #[test]
    fn has_full_schema_delegates_to_tools() {
        assert!(tools::has_full_schema("read_file"));
        assert!(tools::has_full_schema("exec"));
        assert!(tools::has_full_schema("get_tool_schema"));
        assert!(!tools::has_full_schema("git"));
        assert!(!tools::has_full_schema("snapshot"));
        assert!(!tools::has_full_schema("ls"));
    }

    #[test]
    fn activate_tool_adds_custom_to_exposed() {
        let mut s = test_session("/workspace");
        assert!(!s.exposed_tools.contains("custom_tool"));
        s.activate_tool("custom_tool");
        assert!(s.exposed_tools.contains("custom_tool"));
    }

    #[test]
    fn activate_all_tools_adds_on_demand() {
        let mut s = test_session("/workspace");
        let before = s.exposed_tools.len();
        s.activate_all_tools();
        for name in tools::on_demand_names() {
            assert!(
                s.exposed_tools.contains(name),
                "activate_all should add {name}"
            );
        }
        assert!(s.exposed_tools.len() > before);
    }

    #[test]
    fn read_cache_bounded_after_many_files() {
        let mut s = test_session("/workspace");
        for i in 0..2000 {
            let path = PathBuf::from(format!("/workspace/file_{i}.txt"));
            s.update_read_cache(path, &format!("content {i}"));
        }
        assert!(
            s.read_cache.len() <= 1024,
            "read_cache should be bounded to a max size, but has {} entries",
            s.read_cache.len()
        );
    }

    #[test]
    fn exec_usage_bounded_after_many_commands() {
        let mut s = test_session("/workspace");
        for i in 0..2000 {
            s.record_exec_usage(format!("cmd_{i}"));
        }
        assert!(
            s.exec_usage.len() <= 1024,
            "exec_usage should be bounded to a max size, but has {} entries",
            s.exec_usage.len()
        );
    }
}
