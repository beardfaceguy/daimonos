//! Explicit-only configuration discovery policy (WP11, FR-09, ADR-021).
//!
//! Context: docs/contexts/reviewer-runtime/CONTEXT.md. The `--ignore-config-files`
//! run option (reviewer-runtime INTERFACES.md, FR-09) refuses every *implicit*
//! configuration source so a hostile workspace or home file cannot configure an
//! opted-in run. The implicit sources are catalogued as D01-D09 in
//! reviewer-runtime INVENTORIES.md:
//!
//! | kind | file(s) | inventory row |
//! |------|---------|---------------|
//! | [`DiscoveryKind::EnvFile`]      | `agent.env`            | D01, D03 |
//! | [`DiscoveryKind::Config`]       | `daimonos.toml` / `config.toml` | D02, D09 (plugins live inside) |
//! | [`DiscoveryKind::Instructions`] | `agent-instructions.md`| D04 |
//! | [`DiscoveryKind::Skills`]       | `.agents/skills`       | D05 |
//! | [`DiscoveryKind::McpServers`]   | `mcp_servers.json`     | D06 |
//! | [`DiscoveryKind::ZedSettings`]  | Zed `settings.json`    | D07 |
//! | [`DiscoveryKind::IgnoreFiles`]  | `.gitignore` / `.ignore` | D08 |
//!
//! Only caller-named explicit inputs (`--config`, `--agent-instructions`, an
//! explicit `--agent-env`, explicitly selected worker/MCP definitions) remain
//! honoured. Invalid explicit inputs fail rather than selecting another source
//! (ADR-021). The effective policy and the ignored source paths are reported at
//! startup without reading their contents. D10 (live remote model discovery) is
//! not disabled here; it is owned by the controlled-transport / offline-tools
//! packages (WP07/WP20).

use std::path::{Path, PathBuf};

/// Which configuration sources a run may read.
///
/// Default is [`DiscoveryPolicy::Implicit`]: the option is default-off and
/// current behaviour is unchanged unless the caller opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoveryPolicy {
    /// Legacy behaviour: implicit workspace and user/home discovery is permitted.
    #[default]
    Implicit,
    /// `--ignore-config-files`: only caller-named explicit inputs are honoured.
    ExplicitOnly,
}

impl DiscoveryPolicy {
    /// Resolve the policy from the `--ignore-config-files` CLI flag.
    pub fn from_ignore_flag(ignore_config_files: bool) -> Self {
        if ignore_config_files {
            Self::ExplicitOnly
        } else {
            Self::Implicit
        }
    }

    /// True when implicit configuration sources are refused.
    pub fn is_explicit_only(self) -> bool {
        matches!(self, Self::ExplicitOnly)
    }

    /// True when ordinary implicit discovery applies (default behaviour).
    pub fn allows_implicit(self) -> bool {
        matches!(self, Self::Implicit)
    }
}

/// One recognised implicit configuration kind that explicit-only mode refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryKind {
    EnvFile,
    Config,
    Instructions,
    Skills,
    McpServers,
    ZedSettings,
    IgnoreFiles,
}

impl DiscoveryKind {
    /// Stable inventory code (reviewer-runtime INVENTORIES.md D01-D09).
    pub fn code(self) -> &'static str {
        match self {
            Self::EnvFile => "D01",
            Self::Config => "D02",
            Self::Instructions => "D04",
            Self::Skills => "D05",
            Self::McpServers => "D06",
            Self::ZedSettings => "D07",
            Self::IgnoreFiles => "D08",
        }
    }

    /// Human-readable label for the startup report.
    pub fn label(self) -> &'static str {
        match self {
            Self::EnvFile => "environment file",
            Self::Config => "config file",
            Self::Instructions => "agent instructions",
            Self::Skills => "skills directory",
            Self::McpServers => "mcp servers file",
            Self::ZedSettings => "zed settings file",
            Self::IgnoreFiles => "ignore file",
        }
    }
}

/// One ignored implicit source, named by path for the startup report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredSource {
    pub kind: DiscoveryKind,
    pub path: PathBuf,
}

fn push_if_exists(out: &mut Vec<IgnoredSource>, kind: DiscoveryKind, path: PathBuf) {
    // `symlink_metadata` stats the path without following links or reading
    // contents, so reporting never trusts a hostile file.
    if std::fs::symlink_metadata(&path).is_ok() {
        out.push(IgnoredSource { kind, path });
    }
}

/// Enumerate the implicit configuration sources that exist on disk and are
/// refused under explicit-only mode. This is the single registry the startup
/// report iterates; adding a newly recognised source is one `push_if_exists`
/// line. Paths are stat-only — their contents are never read.
///
/// `explicit_config` is the caller's `--config` path (if any); it is honoured,
/// so a workspace/home config candidate that equals it is not reported as
/// ignored.
pub fn ignored_sources(workspace: &Path, explicit_config: Option<&Path>) -> Vec<IgnoredSource> {
    let mut out = Vec::new();
    let daimonos_config_dir = crate::paths::config_dir().map(|dir| dir.join("daimonos"));
    let home = crate::paths::home_dir();

    // D01/D03 — environment file (dotenv-style `agent.env`).
    push_if_exists(
        &mut out,
        DiscoveryKind::EnvFile,
        workspace.join("agent.env"),
    );
    if let Some(dir) = &daimonos_config_dir {
        push_if_exists(&mut out, DiscoveryKind::EnvFile, dir.join("agent.env"));
    }

    // D02/D09 — config file (plugin/tool registrations live inside it).
    let workspace_config = workspace.join("daimonos.toml");
    if explicit_config != Some(workspace_config.as_path()) {
        push_if_exists(&mut out, DiscoveryKind::Config, workspace_config);
    }
    if let Some(dir) = &daimonos_config_dir {
        let user_config = dir.join("config.toml");
        if explicit_config != Some(user_config.as_path()) {
            push_if_exists(&mut out, DiscoveryKind::Config, user_config);
        }
    }

    // D04 — agent instruction / prompt files.
    push_if_exists(
        &mut out,
        DiscoveryKind::Instructions,
        workspace.join("agent-instructions.md"),
    );
    if let Some(dir) = &daimonos_config_dir {
        push_if_exists(
            &mut out,
            DiscoveryKind::Instructions,
            dir.join("agent-instructions.md"),
        );
    }

    // D05 — workspace and global skill roots.
    push_if_exists(
        &mut out,
        DiscoveryKind::Skills,
        workspace.join(crate::skills::DEFAULT_SKILL_DIR),
    );
    if let Ok(global) = crate::skills::global_root_from(None, home.as_deref()) {
        push_if_exists(&mut out, DiscoveryKind::Skills, global);
    }

    // D06 — default MCP servers file.
    if let Some(dir) = &daimonos_config_dir {
        push_if_exists(
            &mut out,
            DiscoveryKind::McpServers,
            dir.join("mcp_servers.json"),
        );
    }

    // D07 — Zed context-server settings.
    push_if_exists(
        &mut out,
        DiscoveryKind::ZedSettings,
        crate::zed_config::default_settings_path(),
    );

    // D08 — ignore files that steer evidence walks.
    for name in [".gitignore", ".ignore"] {
        push_if_exists(&mut out, DiscoveryKind::IgnoreFiles, workspace.join(name));
    }

    out
}

/// Build the `effective_policy` startup report value.
pub fn report_value(ignored: &[IgnoredSource]) -> serde_json::Value {
    serde_json::json!({
        "effective_policy": {
            "ignore_config_files": true,
            "ignored": ignored
                .iter()
                .map(|source| serde_json::json!({
                    "kind": source.kind.code(),
                    "label": source.kind.label(),
                    "path": source.path.display().to_string(),
                }))
                .collect::<Vec<_>>(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_defaults_to_implicit_and_opts_in_on_flag() {
        assert_eq!(DiscoveryPolicy::default(), DiscoveryPolicy::Implicit);
        assert!(DiscoveryPolicy::default().allows_implicit());
        assert!(!DiscoveryPolicy::from_ignore_flag(false).is_explicit_only());
        assert!(DiscoveryPolicy::from_ignore_flag(true).is_explicit_only());
    }

    #[test]
    fn report_lists_present_workspace_sources_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("daimonos.toml"), "").unwrap();
        std::fs::write(ws.join("agent.env"), "").unwrap();
        std::fs::write(ws.join("agent-instructions.md"), "").unwrap();
        std::fs::write(ws.join(".gitignore"), "").unwrap();
        let sources = ignored_sources(ws, None);
        let has = |kind: DiscoveryKind, name: &str| {
            sources
                .iter()
                .any(|s| s.kind == kind && s.path.ends_with(name))
        };
        assert!(has(DiscoveryKind::Config, "daimonos.toml"));
        assert!(has(DiscoveryKind::EnvFile, "agent.env"));
        assert!(has(DiscoveryKind::Instructions, "agent-instructions.md"));
        assert!(has(DiscoveryKind::IgnoreFiles, ".gitignore"));
    }

    #[test]
    fn explicit_config_is_not_reported_as_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let explicit = ws.join("daimonos.toml");
        std::fs::write(&explicit, "").unwrap();
        let sources = ignored_sources(ws, Some(&explicit));
        assert!(
            !sources
                .iter()
                .any(|s| s.kind == DiscoveryKind::Config && s.path == explicit),
            "the caller's explicit config must not be reported ignored"
        );
    }

    #[test]
    fn report_value_shape_is_stable() {
        let ignored = vec![IgnoredSource {
            kind: DiscoveryKind::Config,
            path: PathBuf::from("/ws/daimonos.toml"),
        }];
        let value = report_value(&ignored);
        assert_eq!(value["effective_policy"]["ignore_config_files"], true);
        assert_eq!(value["effective_policy"]["ignored"][0]["kind"], "D02");
        assert_eq!(
            value["effective_policy"]["ignored"][0]["path"],
            "/ws/daimonos.toml"
        );
    }
}
