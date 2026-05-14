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
/// fallback used by `script::script_semaphore()` when
/// `configure_max_concurrent` was never called stays in sync with the
/// config default and the value in `daimonos.default.toml`.
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
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 600,
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
pub fn load(explicit: Option<&Path>, workspace: &Path) -> Config {
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
                        eprintln!("config: loaded from {:?}", candidate);
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

    eprintln!("config: using built-in defaults");
    Config::default()
}

fn dirs_next() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
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
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn default_toml_parses_successfully() {
        let toml_str = include_str!("../daimonos.default.toml");
        let cfg: Config = toml::from_str(toml_str)
            .expect("daimonos.default.toml must parse as valid Config");
        assert_eq!(cfg.process.exec_output_max_chars, 100_000);
        assert_eq!(cfg.process.poll_tail_lines, 20);
        assert_eq!(cfg.index.max_depth, 20);
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

        let cfg = load(Some(&cfg_path), dir.path());
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

        let cfg = load(None, dir.path());
        assert_eq!(cfg.process.poll_tail_lines, 50);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(None, dir.path());
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
}

/// Register tools from config into the tool registry.
pub async fn register_tools(cfg: &Config, registry: &ToolRegistry) {
    for (id, tool_cfg) in &cfg.tools {
        if id == "x07" {
            let plugin = Arc::new(X07Plugin::new(&tool_cfg.bin));
            eprintln!("tools: registered x07 plugin ({})", tool_cfg.bin);
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
            eprintln!("tools: registered generic plugin '{}'", id);
            registry.register(plugin).await;
        }
    }
}
