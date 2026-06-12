use crate::plugins::generic_cli::GenericCliPlugin;
use crate::plugins::x07::X07Plugin;
use crate::tool_runner::ToolRegistry;
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
    pub analytics: AnalyticsConfig,
    pub pipeline_cache: PipelineCacheConfig,
    pub mcp: McpConfig,
    pub discord: DiscordConfig,
    #[serde(default)]
    pub tools: HashMap<String, ToolConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolConfig {
    pub bin: String,
    #[serde(default)]
    pub source_pattern: Option<String>,
    #[serde(default)]
    pub manifest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub max_depth: usize,
    pub max_file_size: usize,
    pub binary_sniff_bytes: usize,
    pub skip_extensions: Vec<String>,
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
}

/// Default cap for `ProcessConfig::max_script_threads`. Exposed so the
/// fallback used inside `script.rs` when `script::configure_max_concurrent`
/// was never called stays in sync with the struct default and the value
/// in `daimonos.default.toml`.
pub const DEFAULT_MAX_SCRIPT_THREADS: usize = 32;

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
        }
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
            let expanded = if let Some(rest) = p.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    std::path::PathBuf::from(home).join(rest)
                } else {
                    std::path::PathBuf::from(p)
                }
            } else {
                std::path::PathBuf::from(p)
            };
            return expanded;
        }
        if let Some(home) = std::env::var_os("HOME") {
            std::path::PathBuf::from(home)
                .join(".daimonos")
                .join("analytics.db")
        } else {
            std::path::PathBuf::from("/tmp/daimonos-analytics.db")
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_depth: 20,
            max_file_size: 1_000_000,
            binary_sniff_bytes: 512,
            skip_extensions: default_skip_extensions(),
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
            extra_path: Vec::new(),
            max_cache_entries: 1024,
            exec_output_filters: true,
            exec_plugin_redirect: true,
            max_script_threads: DEFAULT_MAX_SCRIPT_THREADS,
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
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
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
    let candidates = [
        explicit.map(|p| p.to_path_buf()),
        Some(workspace.join("daimonos.toml")),
        dirs_next().map(|d| d.join("daimonos").join("config.toml")),
    ];

    for candidate in candidates.iter().flatten() {
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

fn dirs_next() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
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

    #[test]
    fn default_config_values() {
        let cfg = Config::default();
        assert_eq!(cfg.index.max_depth, 20);
        assert_eq!(cfg.index.max_file_size, 1_000_000);
        assert_eq!(cfg.search.default_grep_max, 100);
        assert_eq!(cfg.search.default_find_max, 20);
        assert_eq!(cfg.process.poll_tail_lines, 20);
        assert_eq!(cfg.process.exec_output_max_chars, 100_000);
        assert!(!cfg.discord.enabled);
        assert_eq!(cfg.discord.bot_token_env_var, "DISCORD_BOT_TOKEN");
        assert_eq!(cfg.discord.api_base_url, "https://discord.com/api/v10");
        assert!(!cfg.mcp.startup_logs);
        assert!(!cfg.mcp.full_tool_schemas);
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn default_toml_parses_successfully() {
        let toml_str = include_str!("../daimonos.default.toml");
        let cfg: Config =
            toml::from_str(toml_str).expect("daimonos.default.toml must parse as valid Config");
        assert_eq!(cfg.process.exec_output_max_chars, 100_000);
        assert_eq!(cfg.process.poll_tail_lines, 20);
        assert_eq!(cfg.index.max_depth, 20);
        assert!(!cfg.mcp.startup_logs);
        assert!(!cfg.mcp.full_tool_schemas);
    }

    #[test]
    fn effective_full_tool_schemas_env_overrides_config() {
        let cfg = Config::default();
        std::env::set_var("DAIMONOS_MCP_FULL_SCHEMAS", "1");
        assert!(effective_full_tool_schemas(&cfg));
        std::env::set_var("DAIMONOS_MCP_FULL_SCHEMAS", "false");
        assert!(!effective_full_tool_schemas(&cfg));
        std::env::remove_var("DAIMONOS_MCP_FULL_SCHEMAS");
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
