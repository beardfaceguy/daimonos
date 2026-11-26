//! Offline producer admission. Context: docs/adr/032-offline-tools-authority.md.
use crate::cli::RuntimeMode;
use crate::state_root::RunFailure;

/// Only the caller can enable this profile. It never comes from a config file.
#[derive(Clone, Copy, Debug, Default)]
pub enum NetworkAuthority {
    #[default]
    Normal,
    Offline,
}

impl NetworkAuthority {
    pub fn from_cli(offline: bool) -> Self {
        if offline {
            Self::Offline
        } else {
            Self::Normal
        }
    }

    pub fn offline(self) -> bool {
        matches!(self, Self::Offline)
    }

    /// Admit only reviewed, in-process evidence adapters (N13–N21).
    pub fn admit_tool(self, name: &str) -> Result<(), crate::path_policy::ToolFailure> {
        if self.offline() && !LOCAL_TOOLS.contains(&name) {
            return Err(crate::path_policy::ToolFailure::offline_tool_forbidden());
        }
        Ok(())
    }

    pub fn admit_mode(self, mode: &RuntimeMode) -> Result<(), RunFailure> {
        if self.offline()
            && !matches!(
                mode,
                RuntimeMode::McpStdio | RuntimeMode::McpSocket(_) | RuntimeMode::ToolContract
            )
        {
            return Err(RunFailure::configuration_invalid(
                "offline_profile_incompatible",
                format!("--offline-tools cannot start {}", mode.log_name()),
            ));
        }
        Ok(())
    }
}

/// All other names, including new tools and general subprocess tools, default to refusal.
pub const LOCAL_TOOLS: &[&str] = &["ls", "read_file", "search", "workspace_info"];
