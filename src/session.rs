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
    /// Set when `exposed_tools` actually grows during a request, so the MCP
    /// layer can emit a `notifications/tools/list_changed` after the call.
    /// Consumed (cleared) via `take_tools_changed` (vikunja #993).
    pub tools_changed: bool,
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
    /// Agent-mail identity bound by a successful `register_agent` call. `None`
    /// means notifications are inactive; daimonos never silently mints one.
    pub coordination_agent_name: Option<String>,
    /// Newest message id already surfaced to the model/UI respectively.
    pub coordination_model_watermark: i64,
    pub coordination_ui_watermark: i64,
    /// Out-of-band metadata produced by the most recent `ops::dispatch` call.
    /// Populated by the MCP layer right before it converts the `Response`
    /// into a `CallToolResult` and consumed by the analytics layer in the
    /// same dispatch turn. Replaces brittle substring matching on the
    /// serialized response text.
    pub last_response_meta: ResponseMeta,
    /// Per-session output verbosity level (vikunja #181). `Session::new` sets
    /// this from the config default only (`cfg.mcp.default_verbosity`); the
    /// `DAIMONOS_MCP_VERBOSITY` env override is applied at the server startup
    /// edges via `config::effective_verbosity`, keeping this constructor free
    /// of process-global env reads (which would otherwise race with env-mutating
    /// tests). Switchable mid-session via the `set_verbosity` tool.
    pub verbosity: crate::verbosity::Verbosity,
    /// Pinned filesystem authority shared by this run (ADR-022).
    pub authority: Option<Arc<crate::path_policy::FilesystemAuthority>>,
    /// Read-only admission table for `--workspace-read-only` (FR-07, ADR-022).
    /// Derived from `cfg.workspace_read_only`; consulted at every dispatch
    /// boundary before an effect begins. Context: src/run_policy.rs.
    pub admission: crate::run_policy::ToolAdmission,
    /// Explicit tool allowlist for `--tool-allowlist` (FR-02, ADR-023). Derived
    /// from `cfg.tool_allowlist`; consulted by name at every dispatch boundary
    /// and model-visible listing. Unrestricted keeps legacy behavior.
    /// Context: src/run_policy.rs::ToolPolicy.
    pub policy: crate::run_policy::ToolPolicy,
    /// Caller's cooperative tool-budget options (FR-19, WP18, ADR-031), copied
    /// from `cfg.tool_budget`. Used to install a fresh operation budget at each
    /// top-level tool call. Context: src/tool_budget.rs.
    pub tool_budget_config: crate::tool_budget::ToolBudgetConfig,
    /// The operation budget in force for the current top-level tool call, or
    /// `None` between calls / when no budget option is set. Installed once per
    /// top-level call and shared (inherited, never reset) by composite children
    /// so delegation cannot multiply the allowance (ADR-031).
    pub tool_budget: Option<Arc<crate::tool_budget::ToolBudget>>,
    /// R3 (B3): the budget hit (code, configured, consumed) recorded IN-BAND by
    /// the budget code when a genuine deadline/input/entry limit fired on this
    /// call, so the run loop records the reserved `limit_hit` audit event from a
    /// trusted signal instead of parsing attacker-influenceable result text.
    /// Interior-mutable so the `&self` budget helpers can set it; drained by
    /// `take_budget_hit` and cleared when a fresh top-level budget is installed.
    pub budget_hit: std::sync::Mutex<Option<BudgetHitRecord>>,
}

pub type BgProcess = crate::managed_process::ManagedBackground;

/// R3 (B3): an in-band cooperative budget hit: `(code, configured, consumed)`.
pub type BudgetHitRecord = (String, Option<u64>, Option<u64>);

impl Session {
    pub fn new(workspace: PathBuf, cfg: Arc<Config>) -> Self {
        let cwd = match &cfg.filesystem_authority {
            Some(authority) if !workspace.starts_with(authority.root()) => {
                authority.root().to_path_buf()
            }
            _ => workspace.clone(),
        };
        let env = Self::build_initial_env(&cfg);
        let snapshot_store =
            SnapshotStore::for_workspace(workspace.clone(), cfg.snapshot_directory.as_ref())
                .with_discovery_policy(cfg.discovery_policy);
        let verbosity = cfg.mcp.default_verbosity;
        let authority = cfg.filesystem_authority.clone();
        // FR-07 (G1/G6): the KGL and snapshot stores are "relocated" only when
        // they resolve outside the workspace; then their tools may proceed
        // read-only. A workspace-relative configured path is treated as NOT
        // relocated (fail-closed).
        let relocated = |dir: Option<&std::path::PathBuf>| {
            dir.map(|d| !crate::run_policy::resolves_inside(&workspace, d))
                .unwrap_or(false)
        };
        let admission = crate::run_policy::ToolAdmission::new(
            cfg.workspace_read_only,
            relocated(cfg.kgl.state_directory.as_ref()),
            relocated(cfg.snapshot_directory.as_ref()),
        );
        let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
        let tool_budget_config = cfg.tool_budget;
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
            tools_changed: false,
            used_tools: HashSet::new(),
            analytics: None,
            external_session_id: None,
            coordination_agent_name: None,
            coordination_model_watermark: 0,
            coordination_ui_watermark: 0,
            last_response_meta: ResponseMeta::default(),
            verbosity,
            authority,
            admission,
            policy,
            tool_budget_config,
            tool_budget: None,
            budget_hit: std::sync::Mutex::new(None),
        }
    }

    /// Install a fresh operation budget for a top-level tool call unless one is
    /// already active (a composite's children inherit the parent's). Returns
    /// `true` when this call installed the budget and must clear it afterwards
    /// (FR-19, WP18, ADR-031). Context: src/tool_budget.rs.
    pub fn begin_tool_operation(&mut self) -> bool {
        if self.tool_budget.is_some() {
            return false;
        }
        // R3/R4: a fresh top-level call starts with no stale in-band hit.
        *self.budget_hit.lock().expect("budget_hit poisoned") = None;
        self.tool_budget = self.tool_budget_config.start();
        self.tool_budget.is_some()
    }

    /// R3 (B3): record an in-band budget hit (the code and its numbers) so the
    /// run loop can write the reserved `limit_hit` event without parsing result
    /// text. Called by the budget helpers and the native readers on a genuine
    /// hit only.
    pub fn note_budget_hit(&self, hit: &crate::tool_budget::BudgetHit) {
        use crate::tool_budget::BudgetHit;
        let record = match hit {
            BudgetHit::Timeout { configured_ms } => (
                "tool_timeout".to_string(),
                Some(*configured_ms),
                Some(*configured_ms),
            ),
            BudgetHit::Input {
                limit, consumed, ..
            } => (
                "tool_input_limit".to_string(),
                Some(*limit),
                Some(*consumed),
            ),
        };
        *self.budget_hit.lock().expect("budget_hit poisoned") = Some(record);
    }

    /// R3 (B3): drain the in-band budget hit recorded during the last dispatch.
    pub fn take_budget_hit(&self) -> Option<BudgetHitRecord> {
        self.budget_hit.lock().expect("budget_hit poisoned").take()
    }

    /// Clear the operation budget if this call owned it. A composite child
    /// (`owned == false`) leaves the parent's budget in place.
    pub fn end_tool_operation(&mut self, owned: bool) {
        if owned {
            self.tool_budget = None;
        }
    }

    /// Charge `bytes` logical input against the active budget. Returns the
    /// typed [`crate::path_policy::ToolFailure`] when the ceiling is exceeded;
    /// a no-op when no byte budget is active.
    pub fn budget_charge_bytes(
        &self,
        bytes: u64,
        argument: &str,
    ) -> Result<(), crate::path_policy::ToolFailure> {
        if let Some(budget) = &self.tool_budget {
            if let Err(hit) = budget.charge_input(bytes) {
                self.note_budget_hit(&hit);
                return Err(hit.into_failure(Some(argument)));
            }
        }
        Ok(())
    }

    /// R2 (B4): refuse `bytes` against the active byte budget WITHOUT charging,
    /// so `read_file` can reject a file by its metadata size before reading it.
    /// Returns the typed `tool_input_limit` when the size alone would exceed the
    /// remaining allowance; a no-op when no byte budget is active.
    pub fn budget_precheck_bytes(
        &self,
        bytes: u64,
        argument: &str,
    ) -> Result<(), crate::path_policy::ToolFailure> {
        if let Some(budget) = &self.tool_budget {
            if let Err(hit) = budget.precheck_input(bytes) {
                self.note_budget_hit(&hit);
                return Err(hit.into_failure(Some(argument)));
            }
        }
        Ok(())
    }

    /// A clone of the active operation budget, for handing the shared `Arc`
    /// into a blocking search worker so the worker itself meters scanned bytes
    /// and checks the deadline (FR-19 F1/F2). `None` when no budget is active.
    pub fn tool_budget(&self) -> Option<std::sync::Arc<crate::tool_budget::ToolBudget>> {
        self.tool_budget.clone()
    }

    /// Install (or clear with `None`) the operation budget directly. Used by
    /// `execute_script` (FR-19 F4): the script owns ONE shared budget and
    /// installs it on the session only inside each inner per-op lock, clearing
    /// it when the lock is released, so the budget never lingers on the shared
    /// session between ops where a concurrent top-level call could inherit it.
    /// `begin_tool_operation` then sees it already installed and inherits.
    pub fn set_operation_budget(
        &mut self,
        budget: Option<std::sync::Arc<crate::tool_budget::ToolBudget>>,
    ) {
        self.tool_budget = budget;
    }

    /// True when ANY operation budget is active (deadline, byte, or entry).
    /// Used to refuse composite routes whose external work cannot be governed
    /// cooperatively (e.g. `tool_pipeline`).
    pub fn budget_active(&self) -> bool {
        self.tool_budget.is_some()
    }

    /// True when the active budget carries an input-byte or directory-entry
    /// ceiling (a logical-input budget a tool must be able to meter).
    pub fn budget_has_input_limits(&self) -> bool {
        self.tool_budget
            .as_ref()
            .is_some_and(|b| b.has_input_limits())
    }

    /// R1 (B1, ADR-031): under an input-byte or directory-entry budget, admit a
    /// tool only if it is on the explicit [`crate::tool_budget::tool_input_budget_admitted`]
    /// allow-list (it meters its input or reads no workspace/external content).
    /// Every other name is REFUSED with the reviewed `tool_policy_unsupported`
    /// (D60), never silently exempted. Keyed on the configured budget, not an
    /// installed one, so every frontend dispatch boundary (the MCP seam, the
    /// agent/chat/ACP loop, and `execute_script`) can consult it uniformly
    /// before a budget is even installed. `None` when no input/entry budget is
    /// set (a bare `--tool-timeout-ms` refuses nothing here) or when `tool` is
    /// admitted.
    pub fn budget_input_refusal(&self, tool: &str) -> Option<crate::path_policy::ToolFailure> {
        let cfg = &self.tool_budget_config;
        let input_or_entry = cfg.input_bytes.is_some() || cfg.max_entries.is_some();
        if input_or_entry && !crate::tool_budget::tool_input_budget_admitted(tool) {
            Some(
                crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
                    Some(tool),
                    "it is not on the input/entry budget allow-list (it reads workspace \
                 or external content that is not metered, or starts external work)",
                ),
            )
        } else {
            None
        }
    }

    /// R1 (ADR-031): the index-answered search (`search mode=files`) reads
    /// cached-index content that is not a scannable byte count, so under an
    /// input/entry budget it is REFUSED regardless of the admission allow-list
    /// (which admits `search` for its byte-metered content mode). Checks the
    /// installed budget (this runs inside `ops::dispatch`, after the budget was
    /// installed). `None` when no input/entry budget is active.
    pub fn budget_index_search_refusal(&self) -> Option<crate::path_policy::ToolFailure> {
        if self.budget_has_input_limits() {
            Some(
                crate::path_policy::ToolFailure::tool_budget_policy_unsupported(
                    Some("search"),
                    "the index (files) search mode reads cached-index content that is not a \
                 scannable byte count",
                ),
            )
        } else {
            None
        }
    }

    /// Charge `entries` directory entries against the active budget.
    pub fn budget_charge_entries(
        &self,
        entries: u64,
        argument: &str,
    ) -> Result<(), crate::path_policy::ToolFailure> {
        if let Some(budget) = &self.tool_budget {
            if let Err(hit) = budget.charge_entries(entries) {
                self.note_budget_hit(&hit);
                return Err(hit.into_failure(Some(argument)));
            }
        }
        Ok(())
    }

    /// Remaining time until the active budget's absolute deadline, to hand to
    /// the managed-subprocess path so external work (exec, git_read) inherits
    /// but never extends it (FR-19, WP18, ADR-031). `None` when no budget or no
    /// deadline is active.
    pub fn budget_remaining(&self) -> Option<std::time::Duration> {
        self.tool_budget.as_ref().and_then(|b| b.remaining())
    }

    /// The typed `tool_timeout` failure for the active budget, used after the
    /// managed-subprocess path reports it stopped external work at the
    /// deadline. `None` when no budget is active.
    pub fn budget_timeout_failure(
        &self,
        argument: &str,
    ) -> Option<crate::path_policy::ToolFailure> {
        self.tool_budget
            .as_ref()
            .map(|b| b.timeout_failure(Some(argument)))
    }

    /// Error if the active budget's absolute deadline has passed.
    pub fn budget_check_deadline(
        &self,
        argument: &str,
    ) -> Result<(), crate::path_policy::ToolFailure> {
        if let Some(budget) = &self.tool_budget {
            if let Err(hit) = budget.check_deadline() {
                self.note_budget_hit(&hit);
                return Err(hit.into_failure(Some(argument)));
            }
        }
        Ok(())
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

    /// Retire every background process owned by this session. Callers should
    /// await this at orderly teardown; `ManagedBackground::drop` remains a
    /// best-effort emergency path for abrupt owner loss.
    pub async fn shutdown_processes(&mut self) {
        let grace = std::time::Duration::from_millis(self.cfg.process.termination_grace_ms);
        let processes: Vec<_> = self
            .bg_processes
            .drain()
            .map(|(_, process)| process)
            .collect();
        futures_util::future::join_all(
            processes
                .into_iter()
                .map(|process| async move { process.terminate(grace).await }),
        )
        .await;
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
    pub fn check_read_cache(
        &self,
        path: &std::path::Path,
        content: &str,
    ) -> Option<&ReadCacheEntry> {
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

    /// Expose a tool so it appears in future list_tools responses. Marks the
    /// tool set dirty only when the name was not already exposed.
    pub fn activate_tool(&mut self, name: &str) {
        if self.exposed_tools.insert(name.to_string()) {
            self.tools_changed = true;
        }
    }

    /// Expose all known tools at once (including on-demand tier 2 tools).
    /// Marks the tool set dirty if any name was newly added.
    pub fn activate_all_tools(&mut self) {
        for name in tools::on_demand_names() {
            if self.exposed_tools.insert(name.to_string()) {
                self.tools_changed = true;
            }
        }
    }

    /// Return and clear the "tool set changed" flag. The MCP layer calls this
    /// after each tools/call and emits `notifications/tools/list_changed` when
    /// it returns true (vikunja #993).
    pub fn take_tools_changed(&mut self) -> bool {
        std::mem::take(&mut self.tools_changed)
    }
}

fn shellexpand_home(path: &str) -> String {
    crate::paths::expand_tilde(path)
        .to_string_lossy()
        .to_string()
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
    if let Some(home) = crate::paths::home_dir() {
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
        if let Some(home) = crate::paths::home_dir() {
            let cargo_bin = home.join(".cargo/bin");
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
    fn new_uses_config_default_verbosity() {
        // Session::new must read only cfg.mcp.default_verbosity (no process env),
        // so it cannot race with env-mutating tests (vikunja #181; codejung
        // finding). The DAIMONOS_MCP_VERBOSITY override is applied at the server
        // startup edges, not in this constructor.
        let mut cfg = Config::default();
        cfg.mcp.default_verbosity = crate::verbosity::Verbosity::Terse;
        let s = Session::new(PathBuf::from("/workspace"), Arc::new(cfg));
        assert_eq!(s.verbosity, crate::verbosity::Verbosity::Terse);
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
    fn activate_tool_sets_and_takes_changed_flag() {
        let mut s = test_session("/workspace");
        assert!(!s.tools_changed, "fresh session is not dirty");

        // Re-activating an already-exposed tool must not mark dirty.
        let existing = tools::initial_exposed_tools().into_iter().next().unwrap();
        s.activate_tool(&existing);
        assert!(!s.tools_changed, "re-exposing existing tool is a no-op");

        // A genuinely new tool marks dirty.
        s.activate_tool("custom_tool");
        assert!(s.tools_changed, "new tool marks the set dirty");

        // take_tools_changed returns then clears.
        assert!(s.take_tools_changed());
        assert!(!s.tools_changed, "flag cleared after take");
        assert!(!s.take_tools_changed(), "second take returns false");
    }

    #[test]
    fn activate_all_tools_sets_changed_flag_once() {
        let mut s = test_session("/workspace");
        s.activate_all_tools();
        assert!(s.take_tools_changed(), "adding on-demand tools marks dirty");
        // Second call adds nothing new, so no change.
        s.activate_all_tools();
        assert!(!s.tools_changed, "re-activating all is a no-op");
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
