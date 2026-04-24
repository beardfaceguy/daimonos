use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub index: IndexConfig,
    pub search: SearchConfig,
    pub process: ProcessConfig,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            index: IndexConfig::default(),
            search: SearchConfig::default(),
            process: ProcessConfig::default(),
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
        "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg",
        "mp3", "mp4", "avi", "mov", "mkv", "flac", "wav", "ogg", "webm",
        "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst",
        "exe", "dll", "so", "dylib", "o", "a", "lib",
        "wasm", "pyc", "pyo", "class",
        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
        "sqlite", "db", "mdb",
        "ttf", "otf", "woff", "woff2", "eot",
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
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
}
