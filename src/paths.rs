//! Process-level path resolution shared by configuration and runtimes.

use std::path::PathBuf;

/// User home directory from `$HOME`.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Base configuration directory: `$XDG_CONFIG_HOME`, else `$HOME/.config`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))
}

/// Expand a leading `~/` using the shared home-directory resolution.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_without_tilde_is_unchanged() {
        assert_eq!(expand_tilde("/tmp/file"), PathBuf::from("/tmp/file"));
        assert_eq!(
            expand_tilde("relative/file"),
            PathBuf::from("relative/file")
        );
    }

    #[test]
    fn tilde_path_uses_shared_home() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_tilde("~/file"), home.join("file"));
        }
    }
}
