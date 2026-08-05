use crate::plugins::generic_cli::GenericCliPlugin;
use crate::plugins::x07::X07Plugin;
use crate::tool_runner::ToolRegistry;
use crate::verbosity::Verbosity;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub index: IndexConfig,
    pub search: SearchConfig,
    pub process: ProcessConfig,
    pub tool_output: ToolOutputConfig,
    pub loop_detector: LoopDetectorConfig,
    pub logging: LoggingConfig,
    pub observability: ObservabilityConfig,
    pub analytics: AnalyticsConfig,
    pub pipeline_cache: PipelineCacheConfig,
    pub mcp: McpConfig,
    pub acp: AcpConfig,
    pub tui: TuiConfig,
    pub discord: DiscordConfig,
    pub kgl: KglConfig,
    pub coordination: CoordinationConfig,
    pub prompts: PromptsConfig,
    #[serde(default)]
    pub tools: HashMap<String, ToolConfig>,
}

/// Runtime overrides for the model-facing prompts (vikunja #974). Each field is
/// an optional path to a file whose contents replace the embedded default in
/// `src/prompts.rs`. `None`/empty → the embedded default. See `prompts/README.md`.
///
/// WARNING: these steer agent behavior and token cost. `summary` also honors the
/// agent-env `DAIMONOS_AGENT_SUMMARY_PROMPT`, which takes precedence over the
/// path here (see `prompts::apply_summary_override`).
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct PromptsConfig {
    /// Core agent system prompt (`daimonos agent` / `chat` / ACP).
    pub agent_system: Option<String>,
    /// Static MCP server instructions (`daimonos --mcp`).
    pub mcp_instructions: Option<String>,
    /// KGL orientation hint (only emitted when KGL auto-index is on).
    pub kgl_hint: Option<String>,
    /// Compaction summarizer system prompt.
    pub summary: Option<String>,
    /// Loop-detector corrective steer template (vikunja #1197).
    pub loop_steer: Option<String>,
    /// Top-level full/terse tool-description catalog.
    pub tool_descriptions: Option<String>,
    /// Additional user instructions loaded at startup for agent/chat/ACP and
    /// appended to the resolved `agent_system`. Runtime-only: populated from
    /// the default file or `--agent-instructions`, never deserialized from TOML.
    #[serde(skip)]
    pub additional_agent_instructions: Option<String>,
    /// Resolved embedded defaults plus any runtime override.
    #[serde(skip)]
    pub resolved_tool_descriptions: crate::tool_descriptions::ToolDescriptions,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolConfig {
    pub bin: String,
    #[serde(default)]
    pub source_pattern: Option<String>,
    #[serde(default)]
    pub manifest: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Eager,
    Lazy,
    #[default]
    Hybrid,
}

pub const INDEX_FALLBACK_MAX_FILES: usize = 50_000;
pub const DEFAULT_INDEX_MAX_WALK_ENTRIES: usize = 100_000;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub mode: IndexMode,
    pub max_depth: usize,
    pub skip_extensions: Vec<String>,
    /// Hard cap on the number of paths the trigram index will hold. The
    /// walk stops once this many files have been collected. Bounds index
    /// RSS on legitimately huge monorepos and bounds cold search. Also doubles
    /// as the over-broad-root preflight budget. 0 uses an internal safety cap.
    pub max_files: usize,
    /// Hard cap on filesystem entries visited by any preflight or index walk.
    pub max_walk_entries: usize,
    /// When true (default), gate eager indexing on a signal rather than a
    /// path blocklist: a root larger than the `max_files` preflight budget
    /// is auto-indexed only if it looks like a real project (one of
    /// `project_markers` is present at the root). This stops daimonos from
    /// crawling gigabytes of an over-broad root — `$HOME`, a NAS mount, a
    /// downloads dir — that an editor inherited as cwd (a single such
    /// instance was observed at ~1.3 GB RSS). A gated root gets an empty
    /// cold index in hybrid mode; file search populates it on demand under the
    /// same cap. Small roots are warmed regardless of markers.
    pub guard_overbroad_roots: bool,
    /// Filenames whose presence at the workspace root marks it as a "real
    /// project" worth eagerly indexing even when it exceeds the preflight
    /// budget. Checked by `guard_overbroad_roots`.
    pub project_markers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub default_grep_max: usize,
    pub default_find_max: usize,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ProcessConfig {
    pub poll_tail_lines: usize,
    /// Max characters for exec stdout/stderr before truncation.
    /// When exceeded, keeps first + last lines with a truncation notice.
    pub exec_output_max_chars: usize,
    /// Read size for each live foreground-exec progress update.
    pub exec_stream_chunk_bytes: usize,
    /// Additional directories to prepend to PATH for exec/bg commands.
    /// Common tool dirs (~/.cargo/bin, ~/.local/bin) are auto-detected;
    /// use this for non-standard locations.
    pub extra_path: Vec<String>,
    /// Max entries in session-level caches (read_cache, exec_usage, pipeline_cache).
    /// When exceeded, oldest/least-used entries are evicted.
    pub max_cache_entries: usize,
    /// Apply semantic output filters to exec commands (test runners, builds,
    /// installers, linters). Extracts only relevant output — e.g. failure
    /// details from test runs, error lines from builds. Set false to disable.
    pub exec_output_filters: bool,
    /// Redirect exec commands to native plugins when a match is found
    /// (e.g. `exec("cargo test")` routes through the cargo plugin for
    /// structured JSON output). Set false to always use raw exec.
    pub exec_plugin_redirect: bool,
    /// Maximum number of concurrently-running Starlark script threads.
    /// Each `execute_script` invocation runs on a dedicated OS thread;
    /// pure-CPU runaway scripts cannot be cancelled, so this bounds the
    /// damage a misbehaving script (or sequence of them) can do. New
    /// `execute_script` calls block until a slot is free, capped by the
    /// caller's script timeout.
    pub max_script_threads: usize,
    /// Enable in-script LLM sub-calls (`llm_query`/`llm_query_batched`,
    /// ADR-008). Off by default; only engaged in agent/chat/ACP mode where a
    /// provider is available.
    pub script_llm_enabled: bool,
    /// Max LLM sub-calls a single `execute_script` run may issue (fan-out +
    /// sequential total). Bounds cost/latency blast radius.
    pub max_script_subcalls: usize,
    /// Max prompts a single `llm_query_batched` call may take.
    pub max_script_subcall_batch: usize,
}

/// Default cap for `ProcessConfig::max_script_threads`. Exposed so the
/// fallback used inside `script.rs` when `script::configure_max_concurrent`
/// was never called stays in sync with the struct default and the value
/// in `daimonos.default.toml`.
pub const DEFAULT_MAX_SCRIPT_THREADS: usize = 32;

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ToolOutputConfig {
    /// Directory for full outputs replaced by bounded model-visible previews.
    /// `None` resolves to `~/.daimonos/tool-output`.
    pub directory: Option<String>,
    /// Maximum UTF-8 bytes visible to the model for one tool result.
    pub max_bytes: usize,
    /// Maximum lines visible to the model for one tool result.
    pub max_lines: usize,
    /// Managed output retention period.
    pub retention_days: u64,
    /// Newest-first budget for prior successful tool results inside one turn.
    pub intra_turn_result_budget_tokens: u64,
    /// Always preserve at least this many most-recent successful results.
    pub intra_turn_keep_recent_results: usize,
    /// Maximum retained string argument size for old edit/write tool calls.
    pub old_argument_max_chars: usize,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            directory: None,
            max_bytes: 50 * 1024,
            max_lines: 2_000,
            retention_days: 7,
            intra_turn_result_budget_tokens: 40_000,
            intra_turn_keep_recent_results: 5,
            old_argument_max_chars: 2_000,
        }
    }
}

/// Deterministic tool retry-storm detection inside one agent turn (vikunja
/// #1197, adapted from Octomind). All windows are model-round counts; a round
/// is one complete parallel batch of tool calls with their results. No LLM
/// call is ever made by the detector.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LoopDetectorConfig {
    /// Master switch; `false` removes the detector from the agent loop.
    pub enabled: bool,
    /// Steer once an identical `(call, result)` pair has repeated for this
    /// many consecutive rounds (any novel pair resets the window). `0`
    /// disables the repeat signal.
    pub repeat_threshold: u32,
    /// Steer once this many consecutive rounds produced no novel
    /// `(call, result)` pair. `0` disables the window signal.
    pub no_novelty_rounds: u32,
    /// Hard-stop the turn after this many consecutive no-novelty rounds.
    /// `0` disables the circuit breaker (steers only).
    pub circuit_breaker_rounds: u32,
}

impl Default for LoopDetectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repeat_threshold: 3,
            no_novelty_rounds: 3,
            circuit_breaker_rounds: 12,
        }
    }
}

// LLM/provider connection config lives in the agent env file, loaded by the
// `agent_env` module (vikunja #949). This TOML section contains only ACP
// protocol-server operational limits.

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AcpConfig {
    /// Maximum saved sessions returned by one ACP session/list response.
    pub session_list_page_size: usize,
    /// MCP-server bridge: consume the MCP servers Zed forwards on
    /// session/new/load and expose their tools to the model (ADR-003, #990).
    pub mcp: AcpMcpConfig,
}

pub const DEFAULT_TUI_HISTORY_ENTRIES: usize = 100;
pub const DEFAULT_TUI_SCROLLBACK_ENTRIES: usize = 2_000;

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TuiConfig {
    /// Process-local submitted prompt history retained for Up/Down navigation.
    pub history_entries: usize,
    /// Maximum transcript and tool-card entries retained in the rendered view.
    pub scrollback_entries: usize,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            history_entries: DEFAULT_TUI_HISTORY_ENTRIES,
            scrollback_entries: DEFAULT_TUI_SCROLLBACK_ENTRIES,
        }
    }
}

impl TuiConfig {
    fn validate(&self) -> Result<(), String> {
        if self.history_entries == 0 {
            return Err("tui.history_entries must be greater than zero".to_string());
        }
        if self.scrollback_entries == 0 {
            return Err("tui.scrollback_entries must be greater than zero".to_string());
        }
        Ok(())
    }
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            session_list_page_size: 50,
            mcp: AcpMcpConfig::default(),
        }
    }
}

impl AcpConfig {
    fn validate(&self) -> Result<(), String> {
        if self.session_list_page_size == 0 {
            return Err("acp.session_list_page_size must be greater than zero".to_string());
        }
        self.mcp.validate()?;
        Ok(())
    }
}

/// Configuration for the ACP MCP-server bridge (ADR-003, vikunja #990). Zed
/// forwards every configured context server on session/new and session/load;
/// when enabled, daimonos connects to each as an MCP client, discovers its
/// tools, and exposes them to the model as `mcp__{server}__{tool}`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AcpMcpConfig {
    /// Master switch for the bridge. When false, forwarded `mcp_servers` are
    /// ignored and the `mcp` agent capability is not advertised.
    pub enabled: bool,
    /// Accept + advertise stdio-transport MCP servers (command/args/env).
    pub allow_stdio: bool,
    /// Accept + advertise HTTP-transport MCP servers (url/headers).
    pub allow_http: bool,
    /// Reuse identical initialized clients across ACP sessions in one process.
    /// Disable for servers that intentionally require per-chat state isolation.
    pub shared_pool_enabled: bool,
    /// Per-server budget (seconds) for the initialize + tools/list handshake.
    /// A server that exceeds it is skipped (fail-open); the session proceeds
    /// with the remaining servers and all native tools.
    pub init_timeout_secs: u64,
    /// Per remote `tools/call` budget (seconds). On timeout the model gets an
    /// error tool result and the turn continues.
    pub call_timeout_secs: u64,
    /// Maximum time to wait for an MCP client runtime/child to shut down.
    /// Teardown continues after the deadline so ACP itself can exit.
    pub shutdown_timeout_secs: u64,
    /// Upper bound on forwarded servers connected per session (bounds spawned
    /// processes / connections). Extra servers are ignored.
    pub max_servers: usize,
    /// Maximum server initialize/list handshakes in flight at once. Results
    /// are still registered in forwarded-server order so tool names remain
    /// deterministic.
    pub max_concurrent_connects: usize,
    /// Upper bound on tools registered from any single server (bounds the
    /// exposed tool set). Extra tools are ignored.
    pub max_tools_per_server: usize,
    /// When Zed forwards an EMPTY MCP server list at session/new|load (a
    /// cold-start race in unpatched Zed: its context-server store isn't
    /// populated when it issues the restored session, and it never re-forwards
    /// to a live session), fall back to reading Zed's own `context_servers`
    /// settings directly and bridge those. Only triggers on an empty forwarded
    /// list — never overrides servers Zed did forward.
    ///
    /// **Opt-in (default false):** it reads an external app's config file and
    /// spawns that config's stdio servers, so it must not fire for non-Zed ACP
    /// clients or in tests. Enable it only when running daimonos as Zed's ACP
    /// agent on an unpatched Zed.
    pub zed_config_fallback: bool,
    /// Path to Zed's `settings.json` for `zed_config_fallback`. `None` derives
    /// it from `$XDG_CONFIG_HOME`/`$HOME` (`~/.config/zed/settings.json`).
    pub zed_settings_path: Option<String>,
}

impl Default for AcpMcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_stdio: true,
            allow_http: true,
            shared_pool_enabled: true,
            init_timeout_secs: 10,
            call_timeout_secs: 60,
            shutdown_timeout_secs: 5,
            max_servers: 32,
            max_concurrent_connects: 8,
            max_tools_per_server: 128,
            zed_config_fallback: false,
            zed_settings_path: None,
        }
    }
}

impl AcpMcpConfig {
    fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.init_timeout_secs == 0 {
            return Err("acp.mcp.init_timeout_secs must be greater than zero".to_string());
        }
        if self.call_timeout_secs == 0 {
            return Err("acp.mcp.call_timeout_secs must be greater than zero".to_string());
        }
        if self.shutdown_timeout_secs == 0 {
            return Err("acp.mcp.shutdown_timeout_secs must be greater than zero".to_string());
        }
        if self.max_servers == 0 {
            return Err("acp.mcp.max_servers must be greater than zero".to_string());
        }
        if self.max_concurrent_connects == 0 {
            return Err("acp.mcp.max_concurrent_connects must be greater than zero".to_string());
        }
        if self.max_tools_per_server == 0 {
            return Err("acp.mcp.max_tools_per_server must be greater than zero".to_string());
        }
        Ok(())
    }
}

impl ProcessConfig {
    fn validate(&self) -> Result<(), String> {
        if self.exec_stream_chunk_bytes == 0 {
            return Err("process.exec_stream_chunk_bytes must be greater than zero".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LoggingConfig {
    /// Persist structured process logs independently of the ACP/MCP client.
    pub enabled: bool,
    /// File-layer filter: trace, debug, info, warn, error, or off.
    pub level: String,
    /// Stderr-layer filter. ACP stdout is never used for logs.
    pub stderr_level: String,
    /// Log directory. `None` resolves through XDG_STATE_HOME, then ~/.local/state.
    pub directory: Option<String>,
    /// Prefix used for rotated log filenames.
    pub file_prefix: String,
    /// Rotation period: hourly, daily, or never.
    pub rotation: String,
    /// Maximum retained rotated files.
    pub max_files: usize,
    /// Seconds between process resource snapshots; 0 disables telemetry.
    pub resource_interval_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ObservabilityConfig {
    /// Export OpenTelemetry traces. Disabled by default.
    pub enabled: bool,
    /// Exact signal-specific OTLP/HTTP traces endpoint.
    pub endpoint: String,
    /// Send RFC 7617 Basic Auth using credentials from the named env vars.
    pub basic_auth: bool,
    /// Environment variable containing the Basic Auth username/public key.
    pub basic_auth_username_env: String,
    /// Environment variable containing the Basic Auth password/secret key.
    pub basic_auth_password_env: String,
    /// Deployment environment attached to traces.
    pub environment: String,
    /// Optional release identifier attached to traces.
    pub release: Option<String>,
    /// Parent-based trace sample ratio in the inclusive range 0.0..=1.0.
    pub sample_ratio: f64,
    /// Maximum spans waiting for background export.
    pub max_queue_size: usize,
    /// Maximum spans sent in one export request.
    pub max_batch_size: usize,
    /// Maximum delay before a partial batch is exported.
    pub batch_delay_ms: u64,
    /// Maximum time allowed for exporter shutdown/flush.
    pub flush_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AnalyticsConfig {
    pub enabled: bool,
    /// Path to SQLite database. Default: ~/.daimonos/analytics.db
    pub db_path: Option<String>,
    /// Days to retain analytics data before auto-cleanup.
    pub retention_days: u64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct McpConfig {
    /// Maximum seconds the MCP server may sit idle (no incoming requests)
    /// before it self-exits. Protects against orphaned subprocesses when
    /// a parent editor leaks the stdin pipe (e.g. closes an agent panel
    /// without sending a shutdown / closing stdin) — without this, the
    /// process blocks in its read loop forever and accumulates resources.
    /// Set to 0 to disable. Overridable at startup via the
    /// `DAIMONOS_IDLE_TIMEOUT_SECS` environment variable (used by tests).
    pub idle_timeout_secs: u64,
    /// When `false` (default), `--mcp` mode avoids informational messages on
    /// stderr during startup and idle shutdown. Cursor (and some other MCP
    /// hosts) classify subprocess stderr as `[error]` in the UI even when the
    /// text is benign (plugin registration lines, indexer stats, watchdog).
    /// Use `--verbose`, set `[mcp] startup_logs = true`, or export
    /// `DAIMONOS_LOG_STARTUP=1` to restore stderr diagnostics.
    pub startup_logs: bool,
    /// When `true`, `list_tools` returns full JSON Schemas for Terse-tier
    /// tools (git, cargo, docker, etc.) instead of empty `{type: object}`.
    /// Default off to save tokens in editor sessions. Enable for Glama
    /// introspection via `[mcp] full_tool_schemas = true` or
    /// `DAIMONOS_MCP_FULL_SCHEMAS=1`.
    pub full_tool_schemas: bool,
    /// Default per-session output verbosity level (vikunja #181): one of
    /// `full` (today's behavior), `compact`, `terse`. Overridable at startup
    /// via `DAIMONOS_MCP_VERBOSITY`; changeable mid-session via the
    /// `set_verbosity` tool. Lower levels trade tool-output detail for tokens.
    pub default_verbosity: Verbosity,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct DiscordConfig {
    /// Enables Discord integration features. When false, Discord settings are ignored.
    pub enabled: bool,
    /// Environment variable name that contains the Discord bot token.
    pub bot_token_env_var: String,
    /// Base URL for the Discord REST API.
    pub api_base_url: String,
    /// Allowed guild IDs. Empty means deny by default until explicitly configured.
    pub allow_guild_ids: Vec<String>,
    /// Allowed channel IDs. Empty means deny by default until explicitly configured.
    pub allow_channel_ids: Vec<String>,
    /// Hard cap for read/search message count per call.
    pub max_messages_per_call: usize,
    /// Max UTF-8 chars per message body retained in responses.
    pub max_message_chars: usize,
    /// Max UTF-8 chars for total serialized response payload.
    pub max_response_chars: usize,
    /// Keep write actions disabled by default.
    pub read_only_default: bool,
    /// Max retry attempts on Discord API 429 responses.
    pub rate_limit_max_retries: usize,
    /// Hard cap for per-retry sleep duration in milliseconds.
    pub rate_limit_max_sleep_ms: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 600,
            startup_logs: false,
            full_tool_schemas: false,
            default_verbosity: Verbosity::Full,
        }
    }
}

/// Tunables for the KGL (knowledge-graph) layer. KGL itself is gated on
/// `DAIMONOS_KGL_AUTOINDEX` / `DAIMONOS_KGL_OBSERVE`; these values govern its
/// SQLite access and the background file-watcher when it is enabled.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct KglConfig {
    /// SQLite busy-timeout in milliseconds applied to every KGL store
    /// connection. The autoindex watcher writes on its own connection while
    /// `kgl_query` / `kgl_assert` use separate ones; a non-zero busy timeout
    /// (with WAL journaling) lets a connection wait briefly for a lock instead
    /// of surfacing `SQLITE_BUSY` as a tool error.
    pub busy_timeout_ms: u64,
    /// Hard cap on inotify watches the KGL file-watcher may register, so it
    /// never exhausts `fs.inotify.max_user_watches`.
    pub max_watches: usize,
    /// Debounce window in seconds: coalesce change bursts into at most one
    /// graph rebuild per tick.
    pub debounce_secs: u64,
    /// Max task-matching defs the `kgl_query orient` bundle expands (each adds
    /// its edges + dependents), bounding the response size of one orient call.
    pub orient_max_matches: usize,
    /// SQL LIMIT applied to every `kgl_query find` result set. Prevents
    /// broad LIKE queries from materialising the entire graph into memory.
    pub find_max: usize,
    /// Hard node cap for `blast_radius` BFS. Prevents dense call graphs from
    /// exhausting CPU/memory during an unbounded transitive traversal.
    pub blast_radius_max: usize,
    /// Directory base names never walked when detecting/indexing a substrate or
    /// registering watches (build/vcs churn + our own store).
    pub skip_dirs: Vec<String>,
}

impl Default for KglConfig {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5_000,
            max_watches: 4_096,
            debounce_secs: 2,
            orient_max_matches: 12,
            find_max: 200,
            blast_radius_max: 500,
            skip_dirs: default_kgl_skip_dirs(),
        }
    }
}

fn default_kgl_skip_dirs() -> Vec<String> {
    [
        "target",
        ".git",
        ".jj",
        "node_modules",
        ".kgl",
        "graphify-out",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Native agent-to-agent coordination ("agent mail") — ADR-009, vikunja #1057.
/// Governs the per-workspace coordination SQLite store shared by concurrent
/// daimonos processes. All fields are optional in TOML (`#[serde(default)]`).
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CoordinationConfig {
    /// Master switch. When false, coordination tools return a soft "disabled"
    /// error and never touch disk (fail-open; ADR-009 D7).
    pub enabled: bool,
    /// SQLite busy-timeout in milliseconds for every coordination connection
    /// (WAL lets concurrent processes proceed; the timeout absorbs contention).
    pub busy_timeout_ms: u64,
    /// Optional override for the coordination base directory. When unset,
    /// defaults to `~/.daimonos/coordination` (the `~/.daimonos/` global-state
    /// convention; the DB lives OUTSIDE the target repo tree).
    pub db_dir: Option<String>,
    /// Default reservation TTL (seconds) when a caller omits one.
    pub default_reservation_ttl_secs: u64,
    /// Hard ceiling on a reservation TTL (seconds); larger requests are clamped.
    pub max_reservation_ttl_secs: u64,
    /// Default `fetch_inbox` page size when the caller omits `limit`.
    pub inbox_default_limit: i64,
    /// Hard ceiling on `fetch_inbox` results, so a caller-supplied `limit`
    /// cannot force an unbounded inbox read (parallels `thread_max_messages`).
    pub inbox_max_limit: i64,
    /// Hard cap on messages returned when reconstructing a thread — bounds the
    /// read so a long/adversarial reply chain cannot blow up a response
    /// (ADR-009 D3/D7; no recursion).
    pub thread_max_messages: i64,
    /// Cooperative unread-mail notifications (ADR-009 amendment, #1063).
    pub notifications: CoordinationNotificationsConfig,
}

/// Safe-boundary coordination notification policy. Model and UI notices are
/// independently deduplicated by per-session newest-message watermarks.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CoordinationNotificationsConfig {
    pub enabled: bool,
    pub model_notice: bool,
    pub ui_notice: bool,
    /// Idle ACP poll interval. Clamped to >= 250ms.
    pub poll_interval_ms: u64,
}

impl CoordinationNotificationsConfig {
    pub fn effective_poll_interval_ms(&self) -> u64 {
        self.poll_interval_ms.max(250)
    }
}

impl Default for CoordinationNotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model_notice: true,
            ui_notice: true,
            poll_interval_ms: 1_500,
        }
    }
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            busy_timeout_ms: 5_000,
            db_dir: None,
            default_reservation_ttl_secs: 3_600,
            max_reservation_ttl_secs: 86_400,
            inbox_default_limit: 20,
            inbox_max_limit: 500,
            thread_max_messages: 500,
            notifications: CoordinationNotificationsConfig::default(),
        }
    }
}

impl CoordinationConfig {
    /// Resolve the coordination base directory: explicit `db_dir` (tilde
    /// expanded), else `~/.daimonos/coordination`, else a `/tmp` fallback if
    /// `$HOME` can't be resolved (never blocks startup — matches analytics).
    pub fn resolved_db_dir(&self) -> std::path::PathBuf {
        if let Some(dir) = &self.db_dir {
            return crate::paths::expand_tilde(dir);
        }
        if let Some(home) = crate::paths::home_dir() {
            home.join(".daimonos").join("coordination")
        } else {
            // $HOME unresolvable: fall back under a per-user temp dir. Namespacing
            // by uid keeps two users from racing on / hijacking / sharing one
            // world-writable coordination DB (the workspace is the trust
            // boundary; a shared /tmp path would breach it). Best-effort: a
            // missing uid degrades to the unscoped name.
            let uid = std::env::var("UID")
                .ok()
                .or_else(|| std::env::var("USER").ok())
                .unwrap_or_default();
            let name = if uid.is_empty() {
                "daimonos-coordination".to_string()
            } else {
                format!("daimonos-coordination-{uid}")
            };
            std::env::temp_dir().join(name)
        }
    }

    /// Busy-timeout clamped to a sane floor so a misconfigured `0` can't make a
    /// contended writer fail immediately with `SQLITE_BUSY` (same class as the
    /// `debounce_secs=0` concern). Callers use this rather than the raw field.
    pub fn effective_busy_timeout_ms(&self) -> u64 {
        self.busy_timeout_ms.max(100)
    }

    /// Inbox page size clamped to at least 1 (a `0`/negative default would make
    /// every fetch return nothing). Callers use this rather than the raw field.
    pub fn effective_inbox_default_limit(&self) -> i64 {
        self.inbox_default_limit.max(1)
    }

    /// Inbox hard ceiling clamped to at least 1. `fetch_inbox` clamps a
    /// caller-supplied `limit` to this so a huge value can't force an
    /// unbounded read.
    pub fn effective_inbox_max_limit(&self) -> i64 {
        self.inbox_max_limit.max(1)
    }
}

/// Whether `list_tools` should expose full JSON Schemas for Terse-tier tools.
/// Env `DAIMONOS_MCP_FULL_SCHEMAS` (`1`/`true`/`yes`/`on`) wins over config.
pub fn effective_full_tool_schemas(cfg: &Config) -> bool {
    if let Ok(raw) = std::env::var("DAIMONOS_MCP_FULL_SCHEMAS") {
        return matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    cfg.mcp.full_tool_schemas
}

/// Effective per-session default verbosity (vikunja #181). Env
/// `DAIMONOS_MCP_VERBOSITY` (`full`/`compact`/`terse`, case-insensitive) wins
/// over config; an unrecognized env value falls back to the config value.
pub fn effective_verbosity(cfg: &Config) -> Verbosity {
    if let Ok(raw) = std::env::var("DAIMONOS_MCP_VERBOSITY") {
        if let Some(v) = Verbosity::from_input(&raw) {
            return v;
        }
    }
    cfg.mcp.default_verbosity
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token_env_var: "DISCORD_BOT_TOKEN".to_string(),
            api_base_url: "https://discord.com/api/v10".to_string(),
            allow_guild_ids: Vec::new(),
            allow_channel_ids: Vec::new(),
            max_messages_per_call: 100,
            max_message_chars: 4_000,
            max_response_chars: 32_000,
            read_only_default: true,
            rate_limit_max_retries: 2,
            rate_limit_max_sleep_ms: 10_000,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PipelineCacheConfig {
    /// Hard cap on directory watches the pipeline cache may register.
    /// On Linux this directly bounds inotify watch usage so a single
    /// daimonos process can't exhaust `fs.inotify.max_user_watches`.
    /// When the cap is reached the watcher stops adding more dirs and
    /// logs a warning; later changes in unwatched dirs won't invalidate
    /// the cache, but the process stays within its kernel-resource budget.
    pub max_watches: usize,
    /// Extra directory base names to skip on top of `.gitignore` rules and
    /// the built-in skip list (`.git`, `node_modules`, `target`, `dist`,
    /// `build`, `out`, `.venv`, `venv`, `__pycache__`, `.cache`, `.next`,
    /// `.nuxt`, `.turbo`, `.tox`, `.mypy_cache`, `.pytest_cache`).
    pub extra_ignore_dirs: Vec<String>,
    /// Hard cap on the number of `(tool_id, command)` results held in the
    /// pipeline cache. When `put()` would exceed this, the oldest entry is
    /// evicted. Operators tuning this trade memory for cache hit rate.
    pub max_entries: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: "info".to_string(),
            stderr_level: "warn".to_string(),
            directory: None,
            file_prefix: "daimonos".to_string(),
            rotation: "daily".to_string(),
            max_files: 14,
            resource_interval_secs: 15,
        }
    }
}

impl LoggingConfig {
    pub fn resolved_directory(&self) -> std::path::PathBuf {
        if let Some(path) = &self.directory {
            return crate::paths::expand_tilde(path);
        }
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            return std::path::PathBuf::from(state_home)
                .join("daimonos")
                .join("logs");
        }
        if let Some(home) = crate::paths::home_dir() {
            return home.join(".local/state/daimonos/logs");
        }
        std::path::PathBuf::from("/tmp/daimonos-logs")
    }

    fn validate(&self) -> Result<(), String> {
        const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error", "off"];
        if !LEVELS.contains(&self.level.as_str()) {
            return Err(format!(
                "logging.level must be one of {}, got '{}'",
                LEVELS.join(", "),
                self.level
            ));
        }
        if !LEVELS.contains(&self.stderr_level.as_str()) {
            return Err(format!(
                "logging.stderr_level must be one of {}, got '{}'",
                LEVELS.join(", "),
                self.stderr_level
            ));
        }
        if !matches!(self.rotation.as_str(), "hourly" | "daily" | "never") {
            return Err(format!(
                "logging.rotation must be hourly, daily, or never, got '{}'",
                self.rotation
            ));
        }
        if self.file_prefix.trim().is_empty() {
            return Err("logging.file_prefix must not be empty".to_string());
        }
        if matches!(self.file_prefix.as_str(), "." | "..")
            || self.file_prefix.contains('/')
            || self.file_prefix.contains('\\')
        {
            return Err(
                "logging.file_prefix must be a filename prefix without path separators".to_string(),
            );
        }
        if self.max_files == 0 {
            return Err("logging.max_files must be greater than zero".to_string());
        }
        Ok(())
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:3000/api/public/otel/v1/traces".to_string(),
            basic_auth: true,
            basic_auth_username_env: "LANGFUSE_PUBLIC_KEY".to_string(),
            basic_auth_password_env: "LANGFUSE_SECRET_KEY".to_string(),
            environment: "development".to_string(),
            release: None,
            sample_ratio: 1.0,
            max_queue_size: 2_048,
            max_batch_size: 512,
            batch_delay_ms: 5_000,
            flush_timeout_ms: 3_000,
        }
    }
}

impl ObservabilityConfig {
    fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let endpoint = reqwest::Url::parse(&self.endpoint)
            .map_err(|error| format!("observability.endpoint is invalid: {error}"))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(
                "observability.endpoint must use http or https for OTLP export".to_string(),
            );
        }
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(
                "observability.endpoint must not contain credentials; use environment variables"
                    .to_string(),
            );
        }
        if endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err(
                "observability.endpoint must not contain query parameters or fragments".to_string(),
            );
        }
        if self.basic_auth && !is_valid_env_var_name(&self.basic_auth_username_env) {
            return Err(format!(
                "observability.basic_auth_username_env must be a valid env var name, got '{}'",
                self.basic_auth_username_env
            ));
        }
        if self.basic_auth && !is_valid_env_var_name(&self.basic_auth_password_env) {
            return Err(format!(
                "observability.basic_auth_password_env must be a valid env var name, got '{}'",
                self.basic_auth_password_env
            ));
        }
        if self.environment.trim().is_empty() {
            return Err("observability.environment must not be empty".to_string());
        }
        if !self.sample_ratio.is_finite() || !(0.0..=1.0).contains(&self.sample_ratio) {
            return Err(
                "observability.sample_ratio must be finite and between 0.0 and 1.0".to_string(),
            );
        }
        if self.max_queue_size == 0 {
            return Err("observability.max_queue_size must be greater than zero".to_string());
        }
        if self.max_batch_size == 0 || self.max_batch_size > self.max_queue_size {
            return Err(
                "observability.max_batch_size must be between 1 and max_queue_size".to_string(),
            );
        }
        if self.batch_delay_ms == 0 {
            return Err("observability.batch_delay_ms must be greater than zero".to_string());
        }
        if self.flush_timeout_ms == 0 {
            return Err("observability.flush_timeout_ms must be greater than zero".to_string());
        }
        Ok(())
    }

    pub fn resolve_basic_auth(&self) -> Result<Option<(String, String)>, String> {
        if !self.basic_auth {
            return Ok(None);
        }
        let username = resolve_nonempty_env(
            &self.basic_auth_username_env,
            "observability Basic Auth username",
        )?;
        let password = resolve_nonempty_env(
            &self.basic_auth_password_env,
            "observability Basic Auth password",
        )?;
        if username.contains(':') {
            return Err(format!(
                "observability Basic Auth username env var '{}' must not contain ':'",
                self.basic_auth_username_env
            ));
        }
        if username.chars().any(char::is_control) {
            return Err(format!(
                "observability Basic Auth username env var '{}' must not contain control characters",
                self.basic_auth_username_env
            ));
        }
        if password.chars().any(char::is_control) {
            return Err(format!(
                "observability Basic Auth password env var '{}' must not contain control characters",
                self.basic_auth_password_env
            ));
        }
        // RFC 7617 defaults to an implementation-defined legacy charset unless
        // a server explicitly advertises UTF-8. Langfuse keys are ASCII, so
        // reject ambiguous credentials instead of producing a header that may
        // decode differently across OTLP collectors.
        if !username.is_ascii() || !password.is_ascii() {
            return Err(
                "observability Basic Auth credentials must contain only ASCII characters"
                    .to_string(),
            );
        }
        Ok(Some((username, password)))
    }
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: None,
            retention_days: 90,
        }
    }
}

impl AnalyticsConfig {
    pub fn resolved_db_path(&self) -> std::path::PathBuf {
        if let Some(p) = &self.db_path {
            return crate::paths::expand_tilde(p);
        }
        if let Some(home) = crate::paths::home_dir() {
            home.join(".daimonos").join("analytics.db")
        } else {
            std::path::PathBuf::from("/tmp/daimonos-analytics.db")
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            mode: IndexMode::Hybrid,
            max_depth: 20,
            skip_extensions: default_skip_extensions(),
            max_files: INDEX_FALLBACK_MAX_FILES,
            max_walk_entries: DEFAULT_INDEX_MAX_WALK_ENTRIES,
            guard_overbroad_roots: true,
            project_markers: default_project_markers(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_grep_max: 100,
            default_find_max: 20,
        }
    }
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            poll_tail_lines: 20,
            exec_output_max_chars: 100_000,
            exec_stream_chunk_bytes: 8_192,
            extra_path: Vec::new(),
            max_cache_entries: 1024,
            exec_output_filters: true,
            exec_plugin_redirect: true,
            max_script_threads: DEFAULT_MAX_SCRIPT_THREADS,
            script_llm_enabled: false,
            max_script_subcalls: 32,
            max_script_subcall_batch: 16,
        }
    }
}

impl Default for PipelineCacheConfig {
    fn default() -> Self {
        Self {
            max_watches: 8192,
            extra_ignore_dirs: Vec::new(),
            max_entries: 1024,
        }
    }
}

impl IndexConfig {
    pub fn skip_set(&self) -> HashSet<String> {
        self.skip_extensions.iter().cloned().collect()
    }

    pub fn effective_max_files(&self) -> usize {
        if self.max_files == 0 {
            INDEX_FALLBACK_MAX_FILES
        } else {
            self.max_files
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.max_walk_entries == 0 {
            return Err("index.max_walk_entries must be > 0".to_string());
        }
        if self.max_walk_entries < self.effective_max_files() {
            return Err(
                "index.max_walk_entries must be >= the effective index.max_files".to_string(),
            );
        }
        Ok(())
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        self.index.validate()?;
        self.acp.validate()?;
        self.tui.validate()?;
        self.process.validate()?;
        self.tool_output.validate()?;
        self.logging.validate()?;
        self.observability.validate()?;
        self.discord.validate()
    }
}

impl DiscordConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_env_var_name(&self.bot_token_env_var) {
            return Err(format!(
                "discord.bot_token_env_var must be a valid env var name, got '{}'",
                self.bot_token_env_var
            ));
        }
        if !(self.api_base_url.starts_with("http://") || self.api_base_url.starts_with("https://"))
        {
            return Err(format!(
                "discord.api_base_url must start with http:// or https://, got '{}'",
                self.api_base_url
            ));
        }

        if self.max_messages_per_call == 0 {
            return Err("discord.max_messages_per_call must be > 0".to_string());
        }
        if self.max_message_chars == 0 {
            return Err("discord.max_message_chars must be > 0".to_string());
        }
        if self.max_response_chars == 0 {
            return Err("discord.max_response_chars must be > 0".to_string());
        }
        if self.rate_limit_max_sleep_ms == 0 {
            return Err("discord.rate_limit_max_sleep_ms must be > 0".to_string());
        }

        for guild in &self.allow_guild_ids {
            if !is_valid_discord_snowflake(guild) {
                return Err(format!(
                    "discord.allow_guild_ids has invalid snowflake id '{}'",
                    guild
                ));
            }
        }
        for channel in &self.allow_channel_ids {
            if !is_valid_discord_snowflake(channel) {
                return Err(format!(
                    "discord.allow_channel_ids has invalid snowflake id '{}'",
                    channel
                ));
            }
        }

        if self.enabled {
            self.resolve_bot_token().map(|_| ())?;
        }
        Ok(())
    }

    pub fn resolve_bot_token(&self) -> Result<String, String> {
        let raw = std::env::var(&self.bot_token_env_var).map_err(|_| {
            format!(
                "discord.enabled=true but env var '{}' is not set",
                self.bot_token_env_var
            )
        })?;
        if raw.trim().is_empty() {
            return Err(format!(
                "discord.enabled=true but env var '{}' is empty",
                self.bot_token_env_var
            ));
        }
        Ok(raw)
    }

    pub fn is_guild_allowed(&self, guild_id: &str) -> bool {
        self.allow_guild_ids.iter().any(|id| id == guild_id)
    }

    pub fn is_channel_allowed(&self, channel_id: &str) -> bool {
        self.allow_channel_ids.iter().any(|id| id == channel_id)
    }

    pub fn redact_sensitive(&self, text: &str) -> String {
        match std::env::var(&self.bot_token_env_var) {
            Ok(token) => redact_secret(text, &token),
            Err(_) => text.to_string(),
        }
    }
}

fn is_valid_discord_snowflake(value: &str) -> bool {
    let len = value.len();
    (17..=20).contains(&len) && value.bytes().all(|b| b.is_ascii_digit())
}

fn is_valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn resolve_nonempty_env(name: &str, purpose: &str) -> Result<String, String> {
    let value =
        std::env::var(name).map_err(|_| format!("{purpose} env var '{name}' is not set"))?;
    if value.trim().is_empty() {
        return Err(format!("{purpose} env var '{name}' is empty"));
    }
    Ok(value)
}

pub fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "[REDACTED]")
}

fn default_skip_extensions() -> Vec<String> {
    [
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg", "mp3", "mp4", "avi", "mov",
        "mkv", "flac", "wav", "ogg", "webm", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst",
        "exe", "dll", "so", "dylib", "o", "a", "lib", "wasm", "pyc", "pyo", "class", "pdf", "doc",
        "docx", "xls", "xlsx", "ppt", "pptx", "sqlite", "db", "mdb", "ttf", "otf", "woff", "woff2",
        "eot",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Filenames that, when present at a workspace root, mark it as a real
/// project worth eagerly indexing even past the preflight budget. Mirrors
/// the project-type markers used to build the MCP instructions, plus VCS
/// directories.
fn default_project_markers() -> Vec<String> {
    [
        ".git",
        ".hg",
        ".svn",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Gemfile",
        "pom.xml",
        "build.gradle",
        "CMakeLists.txt",
        "Makefile",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Load config from the first file found in search order:
/// 1. Explicit path (--config flag)
/// 2. <workspace>/daimonos.toml
/// 3. ~/.config/daimonos/config.toml
/// 4. Built-in defaults
///
/// When `quiet_diagnostic_stderr` is true, suppress informational messages for
/// successful config discovery (`loaded from …`, `using built-in defaults`).
/// Parse/read failures are always printed — those are real errors.
pub fn load(explicit: Option<&Path>, workspace: &Path, quiet_diagnostic_stderr: bool) -> Config {
    for candidate in search_candidates(explicit, workspace) {
        let candidate = &candidate;
        if candidate.is_file() {
            match std::fs::read_to_string(candidate) {
                Ok(content) => match toml::from_str::<Config>(&content) {
                    Ok(cfg) => {
                        if !quiet_diagnostic_stderr {
                            eprintln!("config: loaded from {:?}", candidate);
                        }
                        return cfg;
                    }
                    Err(e) => {
                        eprintln!("config: parse error in {:?}: {}", candidate, e);
                    }
                },
                Err(e) => {
                    eprintln!("config: read error for {:?}: {}", candidate, e);
                }
            }
        }
    }

    if !quiet_diagnostic_stderr {
        eprintln!("config: using built-in defaults");
    }
    Config::default()
}

/// Ordered config-file search candidates, matching `load`'s discovery order:
/// explicit `--config` path (if any), then `<workspace>/daimonos.toml`, then
/// `$XDG_CONFIG_HOME`/`~/.config` `daimonos/config.toml`. Candidates that can't
/// be formed (no `$HOME`/`$XDG_CONFIG_HOME`) are omitted. This is the single
/// source of truth shared by `load` and the `--print-config-path` CLI flag.
pub fn search_candidates(explicit: Option<&Path>, workspace: &Path) -> Vec<std::path::PathBuf> {
    [
        explicit.map(|p| p.to_path_buf()),
        Some(workspace.join("daimonos.toml")),
        dirs_next().map(|d| d.join("daimonos").join("config.toml")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Base config directory (`$XDG_CONFIG_HOME`, else `~/.config`). `daimonos`'s
/// own files live under `<this>/daimonos/`. Shared by config discovery and the
/// prompt-scaffold default directory so both agree on where daimonos config
/// lives.
pub(crate) fn dirs_next() -> Option<std::path::PathBuf> {
    crate::paths::config_dir()
}

/// Register tools from config into the tool registry.
pub async fn register_tools(cfg: &Config, registry: &ToolRegistry, quiet_stderr: bool) {
    for (id, tool_cfg) in &cfg.tools {
        if id == "x07" {
            let plugin = Arc::new(X07Plugin::new(&tool_cfg.bin));
            if !quiet_stderr {
                eprintln!("tools: registered x07 plugin ({})", tool_cfg.bin);
            }
            registry.register(plugin).await;
        } else {
            use crate::tool_runner::{ToolCommand, ToolDescriptor};
            let mut commands = HashMap::new();
            commands.insert(
                "run".to_string(),
                ToolCommand {
                    bin: tool_cfg.bin.clone(),
                    args: Vec::new(),
                    output: "json".to_string(),
                },
            );
            let descriptor = ToolDescriptor {
                id: id.clone(),
                commands,
                source_pattern: tool_cfg.source_pattern.clone(),
                manifest: tool_cfg.manifest.clone(),
                diagnostics_format: "json".to_string(),
                supports_quickfix: false,
                quickfix_format: None,
            };
            let plugin = Arc::new(GenericCliPlugin::new(descriptor));
            if !quiet_stderr {
                eprintln!("tools: registered generic plugin '{}'", id);
            }
            registry.register(plugin).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global environment variables so the
    /// parallel test runner can't observe a half-applied env mutation.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn default_config_values() {
        let cfg = Config::default();
        assert_eq!(cfg.index.max_depth, 20);
        assert_eq!(cfg.index.mode, IndexMode::Hybrid);
        assert_eq!(cfg.index.max_files, 50_000);
        assert_eq!(cfg.index.max_walk_entries, 100_000);
        assert_eq!(cfg.search.default_grep_max, 100);
        assert_eq!(cfg.search.default_find_max, 20);
        assert_eq!(cfg.acp.session_list_page_size, 50);
        assert_eq!(cfg.tui.history_entries, DEFAULT_TUI_HISTORY_ENTRIES);
        assert_eq!(cfg.tui.scrollback_entries, DEFAULT_TUI_SCROLLBACK_ENTRIES);
        assert_eq!(cfg.process.poll_tail_lines, 20);
        assert_eq!(cfg.tui.history_entries, 100);
        assert_eq!(cfg.tui.scrollback_entries, 2_000);
        assert_eq!(cfg.process.exec_output_max_chars, 100_000);
        assert_eq!(cfg.process.exec_stream_chunk_bytes, 8_192);
        assert!(cfg.logging.enabled);
        assert_eq!(cfg.logging.level, "info");
        assert_eq!(cfg.logging.stderr_level, "warn");
        assert_eq!(cfg.logging.rotation, "daily");
        assert_eq!(cfg.logging.max_files, 14);
        assert_eq!(cfg.logging.resource_interval_secs, 15);
        assert!(!cfg.observability.enabled);
        assert_eq!(
            cfg.observability.endpoint,
            "http://localhost:3000/api/public/otel/v1/traces"
        );
        assert!(cfg.observability.basic_auth);
        assert_eq!(cfg.observability.sample_ratio, 1.0);
        assert_eq!(cfg.observability.max_queue_size, 2_048);
        assert_eq!(cfg.observability.max_batch_size, 512);
        assert_eq!(cfg.observability.batch_delay_ms, 5_000);
        assert_eq!(cfg.observability.flush_timeout_ms, 3_000);
        assert!(!cfg.discord.enabled);
        assert_eq!(cfg.discord.bot_token_env_var, "DISCORD_BOT_TOKEN");
        assert_eq!(cfg.discord.api_base_url, "https://discord.com/api/v10");
        assert!(!cfg.mcp.startup_logs);
        assert!(!cfg.mcp.full_tool_schemas);
        assert_eq!(cfg.acp.session_list_page_size, 50);
        assert_eq!(cfg.kgl.busy_timeout_ms, 5_000);
        assert_eq!(cfg.kgl.max_watches, 4_096);
        assert_eq!(cfg.kgl.debounce_secs, 2);
        assert_eq!(cfg.kgl.orient_max_matches, 12);
        assert_eq!(cfg.kgl.find_max, 200);
        assert_eq!(cfg.kgl.blast_radius_max, 500);
        assert!(cfg.kgl.skip_dirs.iter().any(|d| d == "node_modules"));
        assert!(cfg.kgl.skip_dirs.iter().any(|d| d == "graphify-out"));
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn default_toml_parses_successfully() {
        let toml_str = include_str!("../daimonos.default.toml");
        let cfg: Config =
            toml::from_str(toml_str).expect("daimonos.default.toml must parse as valid Config");
        assert_eq!(cfg.process.exec_output_max_chars, 100_000);
        assert_eq!(cfg.process.exec_stream_chunk_bytes, 8_192);
        assert_eq!(cfg.process.poll_tail_lines, 20);
        assert_eq!(cfg.index.mode, IndexMode::Hybrid);
        assert_eq!(cfg.index.max_walk_entries, 100_000);
        assert_eq!(cfg.tool_output.max_bytes, 50 * 1024);
        assert_eq!(cfg.tool_output.max_lines, 2_000);
        assert_eq!(cfg.tool_output.retention_days, 7);
        assert_eq!(cfg.tool_output.intra_turn_result_budget_tokens, 40_000);
        assert_eq!(cfg.tool_output.intra_turn_keep_recent_results, 5);
        assert_eq!(cfg.tool_output.old_argument_max_chars, 2_000);
        assert_eq!(cfg.index.max_depth, 20);
        assert!(cfg.logging.enabled);
        assert_eq!(cfg.logging.level, "info");
        assert_eq!(cfg.logging.max_files, 14);
        assert!(!cfg.observability.enabled);
        assert!(cfg.observability.basic_auth);
        assert_eq!(
            cfg.observability.basic_auth_username_env,
            "LANGFUSE_PUBLIC_KEY"
        );
        assert_eq!(
            cfg.observability.basic_auth_password_env,
            "LANGFUSE_SECRET_KEY"
        );
        assert!(!cfg.mcp.startup_logs);
        assert!(!cfg.mcp.full_tool_schemas);
        assert_eq!(cfg.kgl.busy_timeout_ms, 5_000);
        assert_eq!(cfg.kgl.max_watches, 4_096);
    }

    #[test]
    fn index_rejects_unbounded_or_incoherent_walk_limits() {
        for (field, toml) in [
            ("index.max_walk_entries", "[index]\nmax_walk_entries = 0\n"),
            (
                "index.max_walk_entries",
                "[index]\nmax_files = 10\nmax_walk_entries = 9\n",
            ),
        ] {
            let cfg: Config = toml::from_str(toml).unwrap();
            assert!(cfg
                .validate()
                .expect_err("unsafe index limit must be rejected")
                .contains(field));
        }
    }

    #[test]
    fn tool_output_rejects_unsafe_limits() {
        for (field, toml) in [
            ("tool_output.max_bytes", "[tool_output]\nmax_bytes = 255\n"),
            ("tool_output.max_lines", "[tool_output]\nmax_lines = 2\n"),
            (
                "tool_output.retention_days",
                "[tool_output]\nretention_days = 0\n",
            ),
            (
                "tool_output.intra_turn_result_budget_tokens",
                "[tool_output]\nintra_turn_result_budget_tokens = 0\n",
            ),
            (
                "tool_output.intra_turn_keep_recent_results",
                "[tool_output]\nintra_turn_keep_recent_results = 0\n",
            ),
            (
                "tool_output.old_argument_max_chars",
                "[tool_output]\nold_argument_max_chars = 19\n",
            ),
        ] {
            let cfg: Config = toml::from_str(toml).unwrap();
            assert!(
                cfg.validate()
                    .expect_err("unsafe tool-output limit must be rejected")
                    .contains(field),
                "{field} validation error must name the field"
            );
        }
    }

    #[test]
    fn logging_validation_rejects_invalid_values() {
        let mut cfg = Config::default();
        cfg.logging.level = "verbose".to_string();
        assert!(cfg.validate().unwrap_err().contains("logging.level"));

        let mut cfg = Config::default();
        cfg.logging.rotation = "weekly".to_string();
        assert!(cfg.validate().unwrap_err().contains("logging.rotation"));

        let mut cfg = Config::default();
        cfg.logging.file_prefix = "../daimonos".to_string();
        assert!(cfg.validate().unwrap_err().contains("logging.file_prefix"));

        let mut cfg = Config::default();
        cfg.logging.max_files = 0;
        assert!(cfg.validate().unwrap_err().contains("logging.max_files"));
    }

    #[test]
    fn logging_directory_honors_explicit_path() {
        let cfg = LoggingConfig {
            directory: Some("/tmp/daimonos-test-logs".to_string()),
            ..LoggingConfig::default()
        };
        assert_eq!(
            cfg.resolved_directory(),
            std::path::PathBuf::from("/tmp/daimonos-test-logs")
        );
    }

    #[test]
    fn observability_validation_rejects_unsafe_or_unbounded_values() {
        let mut disabled = Config::default();
        disabled.observability.endpoint = "placeholder".to_string();
        disabled.observability.basic_auth_username_env = "INVALID-NAME".to_string();
        assert!(disabled.validate().is_ok());

        let mut cfg = Config::default();
        cfg.observability.enabled = true;
        cfg.observability.endpoint = "file:///tmp/spans".to_string();
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("observability.endpoint"));

        let mut cfg = Config::default();
        cfg.observability.enabled = true;
        cfg.observability.endpoint =
            "https://user:secret@example.com/api/public/otel/v1/traces".to_string();
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("must not contain credentials"));

        let mut cfg = Config::default();
        cfg.observability.enabled = true;
        cfg.observability.sample_ratio = f64::NAN;
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("observability.sample_ratio"));

        let mut cfg = Config::default();
        cfg.observability.enabled = true;
        cfg.observability.max_queue_size = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("observability.max_queue_size"));

        let mut cfg = Config::default();
        cfg.observability.enabled = true;
        cfg.observability.max_batch_size = cfg.observability.max_queue_size + 1;
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("observability.max_batch_size"));

        let mut cfg = Config::default();
        cfg.observability.enabled = true;
        cfg.observability.basic_auth_username_env = "INVALID-NAME".to_string();
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("basic_auth_username_env"));
    }

    #[test]
    fn effective_full_tool_schemas_env_overrides_config() {
        // Process-global env is shared across the parallel test runner, so
        // serialize the (set -> assert -> restore) sequence against any other
        // test that mutates the same variable.
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::default();
        std::env::set_var("DAIMONOS_MCP_FULL_SCHEMAS", "1");
        assert!(effective_full_tool_schemas(&cfg));
        std::env::set_var("DAIMONOS_MCP_FULL_SCHEMAS", "false");
        assert!(!effective_full_tool_schemas(&cfg));
        std::env::remove_var("DAIMONOS_MCP_FULL_SCHEMAS");
    }

    #[test]
    fn default_verbosity_is_full() {
        let cfg = Config::default();
        assert_eq!(cfg.mcp.default_verbosity, Verbosity::Full);
    }

    #[test]
    fn effective_verbosity_env_overrides_config() {
        // Serialize the (set -> assert -> restore) sequence against any other
        // test mutating process-global env (see effective_full_tool_schemas).
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = Config::default();
        cfg.mcp.default_verbosity = Verbosity::Full;

        std::env::set_var("DAIMONOS_MCP_VERBOSITY", "terse");
        assert_eq!(effective_verbosity(&cfg), Verbosity::Terse);
        std::env::set_var("DAIMONOS_MCP_VERBOSITY", "COMPACT");
        assert_eq!(effective_verbosity(&cfg), Verbosity::Compact);

        // Unrecognized env value falls back to the config value.
        std::env::set_var("DAIMONOS_MCP_VERBOSITY", "loud");
        assert_eq!(effective_verbosity(&cfg), Verbosity::Full);

        std::env::remove_var("DAIMONOS_MCP_VERBOSITY");
        assert_eq!(effective_verbosity(&cfg), Verbosity::Full);
    }

    #[test]
    fn mcp_default_verbosity_parses_from_toml() {
        let toml = "[mcp]\ndefault_verbosity = \"terse\"\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.mcp.default_verbosity, Verbosity::Terse);
    }

    #[test]
    fn acp_session_list_page_size_parses_and_must_be_positive() {
        let cfg: Config = toml::from_str("[acp]\nsession_list_page_size = 25\n").unwrap();
        assert_eq!(cfg.acp.session_list_page_size, 25);
        assert!(cfg.validate().is_ok());

        let invalid: Config = toml::from_str("[acp]\nsession_list_page_size = 0\n").unwrap();
        assert!(invalid
            .validate()
            .expect_err("zero page size must be rejected")
            .contains("acp.session_list_page_size"));
    }

    #[test]
    fn tui_limits_parse_and_must_be_positive() {
        let cfg: Config =
            toml::from_str("[tui]\nhistory_entries = 25\nscrollback_entries = 500\n").unwrap();
        assert_eq!(cfg.tui.history_entries, 25);
        assert_eq!(cfg.tui.scrollback_entries, 500);
        assert!(cfg.validate().is_ok());

        for toml in [
            "[tui]\nhistory_entries = 0\n",
            "[tui]\nscrollback_entries = 0\n",
        ] {
            let invalid: Config = toml::from_str(toml).unwrap();
            assert!(invalid.validate().unwrap_err().contains("tui."));
        }
    }

    #[test]
    fn acp_mcp_defaults_are_enabled_and_valid() {
        let cfg = Config::default();
        assert!(cfg.acp.mcp.enabled);
        assert!(cfg.acp.mcp.allow_stdio);
        assert!(cfg.acp.mcp.allow_http);
        assert!(cfg.acp.mcp.shared_pool_enabled);
        assert_eq!(cfg.acp.mcp.init_timeout_secs, 10);
        assert_eq!(cfg.acp.mcp.call_timeout_secs, 60);
        assert_eq!(cfg.acp.mcp.shutdown_timeout_secs, 5);
        assert!(cfg.acp.mcp.max_servers > 0);
        assert_eq!(cfg.acp.mcp.max_concurrent_connects, 8);
        assert!(cfg.acp.mcp.max_tools_per_server > 0);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn acp_mcp_parses_overrides() {
        let cfg: Config = toml::from_str(
            "[acp.mcp]\nenabled = false\nallow_http = false\nshared_pool_enabled = false\ninit_timeout_secs = 3\nshutdown_timeout_secs = 2\nmax_servers = 4\nmax_concurrent_connects = 2\n",
        )
        .unwrap();
        assert!(!cfg.acp.mcp.enabled);
        assert!(!cfg.acp.mcp.allow_http);
        assert!(!cfg.acp.mcp.shared_pool_enabled);
        assert!(cfg.acp.mcp.allow_stdio);
        assert_eq!(cfg.acp.mcp.init_timeout_secs, 3);
        assert_eq!(cfg.acp.mcp.shutdown_timeout_secs, 2);
        assert_eq!(cfg.acp.mcp.max_servers, 4);
        assert_eq!(cfg.acp.mcp.max_concurrent_connects, 2);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn acp_mcp_rejects_zero_timeouts_and_bounds_when_enabled() {
        for (field, toml) in [
            (
                "acp.mcp.init_timeout_secs",
                "[acp.mcp]\ninit_timeout_secs = 0\n",
            ),
            (
                "acp.mcp.call_timeout_secs",
                "[acp.mcp]\ncall_timeout_secs = 0\n",
            ),
            (
                "acp.mcp.shutdown_timeout_secs",
                "[acp.mcp]\nshutdown_timeout_secs = 0\n",
            ),
            ("acp.mcp.max_servers", "[acp.mcp]\nmax_servers = 0\n"),
            (
                "acp.mcp.max_concurrent_connects",
                "[acp.mcp]\nmax_concurrent_connects = 0\n",
            ),
            (
                "acp.mcp.max_tools_per_server",
                "[acp.mcp]\nmax_tools_per_server = 0\n",
            ),
        ] {
            let cfg: Config = toml::from_str(toml).unwrap();
            assert!(cfg
                .validate()
                .expect_err("zero value must be rejected when enabled")
                .contains(field));
        }
    }

    #[test]
    fn acp_mcp_disabled_skips_bound_validation() {
        // With the bridge disabled, zero timeouts/bounds are irrelevant and
        // must not fail startup validation.
        let cfg: Config =
            toml::from_str("[acp.mcp]\nenabled = false\ninit_timeout_secs = 0\nmax_servers = 0\n")
                .unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn exec_stream_chunk_bytes_parses_and_must_be_positive() {
        let cfg: Config = toml::from_str("[process]\nexec_stream_chunk_bytes = 1024\n").unwrap();
        assert_eq!(cfg.process.exec_stream_chunk_bytes, 1024);
        assert!(cfg.validate().is_ok());

        let invalid: Config = toml::from_str("[process]\nexec_stream_chunk_bytes = 0\n").unwrap();
        assert!(invalid
            .validate()
            .expect_err("zero chunk size must be rejected")
            .contains("process.exec_stream_chunk_bytes"));
    }

    #[test]
    fn skip_extensions_includes_common_binaries() {
        let cfg = Config::default();
        let skip = cfg.index.skip_set();
        assert!(skip.contains("png"));
        assert!(skip.contains("exe"));
        assert!(skip.contains("zip"));
        assert!(skip.contains("wasm"));
        assert!(!skip.contains("rs"));
        assert!(!skip.contains("toml"));
    }

    #[test]
    fn load_from_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("test.toml");
        std::fs::write(
            &cfg_path,
            r#"
[search]
default_grep_max = 42
"#,
        )
        .unwrap();

        let cfg = load(Some(&cfg_path), dir.path(), false);
        assert_eq!(cfg.search.default_grep_max, 42);
        assert_eq!(cfg.search.default_find_max, 20);
    }

    #[test]
    fn load_from_workspace_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("daimonos.toml"),
            r#"
[process]
poll_tail_lines = 50
"#,
        )
        .unwrap();

        let cfg = load(None, dir.path(), false);
        assert_eq!(cfg.process.poll_tail_lines, 50);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(None, dir.path(), false);
        assert_eq!(cfg.index.max_depth, 20);
    }

    #[test]
    fn search_candidates_order_and_explicit() {
        let ws = std::path::Path::new("/tmp/ws");
        // Without --config: workspace file comes first, home config after.
        let without = search_candidates(None, ws);
        assert_eq!(without.first().unwrap(), &ws.join("daimonos.toml"));
        assert!(without
            .iter()
            .all(|c| c != std::path::Path::new("/explicit/cfg.toml")));
        // With --config: the explicit path is first in the search order.
        let explicit = std::path::Path::new("/explicit/cfg.toml");
        let with = search_candidates(Some(explicit), ws);
        assert_eq!(with.first().unwrap(), explicit);
        assert_eq!(with[1], ws.join("daimonos.toml"));
    }

    #[test]
    fn parse_toml_with_tools() {
        let toml_str = r#"
[tools.mytest]
bin = "/usr/bin/test"
source_pattern = "*.rs"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.tools.contains_key("mytest"));
        assert_eq!(cfg.tools["mytest"].bin, "/usr/bin/test");
        assert_eq!(cfg.tools["mytest"].source_pattern.as_deref(), Some("*.rs"));
    }

    #[test]
    fn parse_toml_with_discord_config() {
        let toml_str = r#"
[discord]
enabled = true
bot_token_env_var = "MY_DISCORD_TOKEN"
api_base_url = "https://discord.com/api/v10"
allow_guild_ids = ["123456789012345678"]
allow_channel_ids = ["223456789012345678"]
max_messages_per_call = 50
max_message_chars = 2000
max_response_chars = 16000
read_only_default = true
rate_limit_max_retries = 3
rate_limit_max_sleep_ms = 5000
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.discord.enabled);
        assert_eq!(cfg.discord.bot_token_env_var, "MY_DISCORD_TOKEN");
        assert_eq!(cfg.discord.api_base_url, "https://discord.com/api/v10");
        assert!(cfg.discord.is_guild_allowed("123456789012345678"));
        assert!(cfg.discord.is_channel_allowed("223456789012345678"));
        assert_eq!(cfg.discord.max_messages_per_call, 50);
        assert_eq!(cfg.discord.rate_limit_max_retries, 3);
        assert_eq!(cfg.discord.rate_limit_max_sleep_ms, 5000);
    }

    #[test]
    fn discord_validation_requires_token_when_enabled() {
        let mut cfg = Config::default();
        cfg.discord.enabled = true;
        cfg.discord.bot_token_env_var = "DAIMONOS_TEST_DISCORD_TOKEN_MISSING".to_string();
        std::env::remove_var(&cfg.discord.bot_token_env_var);

        let err = cfg.validate().unwrap_err();
        assert!(err.contains("DAIMONOS_TEST_DISCORD_TOKEN_MISSING"));
    }

    #[test]
    fn discord_validation_rejects_invalid_allowlist_ids() {
        let mut cfg = Config::default();
        cfg.discord.enabled = false;
        cfg.discord.allow_channel_ids = vec!["not-a-snowflake".to_string()];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("discord.allow_channel_ids"));
    }

    #[test]
    fn discord_validation_rejects_invalid_api_base_url() {
        let mut cfg = Config::default();
        cfg.discord.api_base_url = "discord.local/api".to_string();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("discord.api_base_url"));
    }

    #[test]
    fn discord_validation_rejects_zero_rate_limit_sleep_cap() {
        let mut cfg = Config::default();
        cfg.discord.rate_limit_max_sleep_ms = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("discord.rate_limit_max_sleep_ms"));
    }

    #[test]
    fn redact_secret_hides_token_value() {
        let redacted = redact_secret("token=abc123", "abc123");
        assert_eq!(redacted, "token=[REDACTED]");
    }

    #[test]
    fn discord_redact_sensitive_hides_env_token() {
        let cfg = Config::default();
        std::env::set_var(&cfg.discord.bot_token_env_var, "discord-super-secret");
        let redacted = cfg
            .discord
            .redact_sensitive("auth failed: discord-super-secret");
        std::env::remove_var(&cfg.discord.bot_token_env_var);
        assert!(!redacted.contains("discord-super-secret"));
        assert!(redacted.contains("[REDACTED]"));
    }
}
