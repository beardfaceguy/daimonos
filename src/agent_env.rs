//! Agent connection config, loaded from a dotenv-style **agent env file**
//! (vikunja #949). `daimonos agent` reads provider/model/base_url/approval/key
//! from this file and **errors out** if the file or any required value is
//! missing — there are no baked-in defaults for these. This is deliberately
//! separate from the TOML `Config` (which governs the rest of the daemon: the
//! MCP server, indexer, kgl, etc.); the agent frontend does not read `[agent]`
//! TOML anymore.
//!
//! Resolution precedence for the file path:
//!   `--agent-env <path>`  >  `$DAIMONOS_AGENT_ENV`  >  `~/.config/daimonos/agent.env`

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Required scalar keys — `daimonos agent` refuses to run if any is absent/empty.
const REQUIRED: &[&str] = &[
    "DAIMONOS_AGENT_PROVIDER",
    "DAIMONOS_AGENT_MODEL",
    "DAIMONOS_AGENT_BASE_URL",
    "DAIMONOS_AGENT_APPROVAL_MODE",
    "DAIMONOS_AGENT_API_KEY",
];

/// Validated agent connection config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnv {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub approval_mode: String,
    pub api_key: String,
    pub allowed_commands: Vec<String>,
    pub denied_commands: Vec<String>,
}

impl AgentEnv {
    /// Resolve the agent env-file path (flag > env var > ~/.config default).
    pub fn resolve_path(flag: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(p) = flag {
            return Some(p);
        }
        if let Some(p) = std::env::var_os("DAIMONOS_AGENT_ENV") {
            return Some(PathBuf::from(p));
        }
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("daimonos")
                .join("agent.env")
        })
    }

    /// Load + validate the agent env file. Returns a clear error (naming the
    /// path and any missing/invalid values) rather than falling back to defaults.
    pub fn load(flag: Option<PathBuf>) -> Result<AgentEnv, String> {
        let path = Self::resolve_path(flag).ok_or_else(|| {
            "cannot resolve agent env file path — set --agent-env or $DAIMONOS_AGENT_ENV, \
             or ensure $HOME is set for ~/.config/daimonos/agent.env"
                .to_string()
        })?;
        let content = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "agent env file not found or unreadable: {} ({e}).\n\
                 Create it (or pass --agent-env <path>) with: {}.",
                path.display(),
                REQUIRED.join(", ")
            )
        })?;
        Self::from_vars(&parse_dotenv(&content), &path)
    }

    fn from_vars(vars: &HashMap<String, String>, path: &Path) -> Result<AgentEnv, String> {
        let present = |k: &str| {
            vars.get(k)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string())
        };

        let missing: Vec<&str> = REQUIRED
            .iter()
            .copied()
            .filter(|k| present(k).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "agent env file {} is missing required value(s): {}",
                path.display(),
                missing.join(", ")
            ));
        }

        // Safe: all REQUIRED confirmed present above.
        let provider = present("DAIMONOS_AGENT_PROVIDER").unwrap();
        let approval_mode = present("DAIMONOS_AGENT_APPROVAL_MODE").unwrap();

        if !matches!(provider.as_str(), "openrouter" | "anthropic") {
            return Err(format!(
                "agent env file {}: DAIMONOS_AGENT_PROVIDER '{}' unsupported (valid: openrouter, anthropic)",
                path.display(),
                provider
            ));
        }
        if !matches!(approval_mode.as_str(), "auto" | "interactive" | "paranoid") {
            return Err(format!(
                "agent env file {}: DAIMONOS_AGENT_APPROVAL_MODE '{}' invalid (valid: auto, interactive, paranoid)",
                path.display(),
                approval_mode
            ));
        }

        Ok(AgentEnv {
            provider,
            model: present("DAIMONOS_AGENT_MODEL").unwrap(),
            base_url: present("DAIMONOS_AGENT_BASE_URL").unwrap(),
            approval_mode,
            api_key: present("DAIMONOS_AGENT_API_KEY").unwrap(),
            allowed_commands: parse_list(vars.get("DAIMONOS_AGENT_ALLOWED_COMMANDS")),
            denied_commands: parse_list(vars.get("DAIMONOS_AGENT_DENIED_COMMANDS")),
        })
    }

    /// Build a `SafetyPolicy` from the approval mode + allow/deny lists.
    pub fn to_safety_policy(
        &self,
        approve_fn: Option<crate::safety::ApproveFn>,
    ) -> crate::safety::SafetyPolicy {
        let approval_mode = match self.approval_mode.as_str() {
            "auto" => crate::safety::ApprovalMode::Auto,
            "paranoid" => crate::safety::ApprovalMode::Paranoid,
            _ => crate::safety::ApprovalMode::Interactive,
        };
        crate::safety::SafetyPolicy {
            approval_mode,
            allowed_commands: self.allowed_commands.clone(),
            denied_commands: self.denied_commands.clone(),
            approve_fn,
        }
    }
}

/// Minimal dotenv parser: `KEY=VALUE` per line; skips blanks and `#` comments;
/// tolerates a leading `export `; strips one layer of surrounding quotes.
fn parse_dotenv(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        let mut val = v.trim();
        if val.len() >= 2
            && ((val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\'')))
        {
            val = &val[1..val.len() - 1];
        }
        map.insert(key, val.to_string());
    }
    map
}

/// Parse an optional comma-separated list into trimmed, non-empty entries.
fn parse_list(raw: Option<&String>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        "DAIMONOS_AGENT_PROVIDER=openrouter\n\
         DAIMONOS_AGENT_MODEL=anthropic/claude-sonnet-4.6\n\
         DAIMONOS_AGENT_BASE_URL=https://openrouter.ai/api/v1\n\
         DAIMONOS_AGENT_APPROVAL_MODE=interactive\n\
         DAIMONOS_AGENT_API_KEY=sk-test\n"
            .to_string()
    }

    fn load_str(s: &str) -> Result<AgentEnv, String> {
        AgentEnv::from_vars(&parse_dotenv(s), Path::new("<test>"))
    }

    #[test]
    fn parses_full_valid_file() {
        let e = load_str(&base()).unwrap();
        assert_eq!(e.provider, "openrouter");
        assert_eq!(e.model, "anthropic/claude-sonnet-4.6");
        assert_eq!(e.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(e.approval_mode, "interactive");
        assert_eq!(e.api_key, "sk-test");
        assert!(e.allowed_commands.is_empty() && e.denied_commands.is_empty());
    }

    #[test]
    fn dotenv_handles_comments_quotes_export_blanks() {
        let s = "# a comment\n\n  export DAIMONOS_AGENT_PROVIDER = \"anthropic\" \n\
                 DAIMONOS_AGENT_MODEL='claude-opus-4-8'\n\
                 DAIMONOS_AGENT_BASE_URL=https://api.anthropic.com\n\
                 DAIMONOS_AGENT_APPROVAL_MODE=auto\n\
                 DAIMONOS_AGENT_API_KEY=abc\n";
        let e = load_str(s).unwrap();
        assert_eq!(e.provider, "anthropic");
        assert_eq!(e.model, "claude-opus-4-8");
        assert_eq!(e.approval_mode, "auto");
        assert_eq!(e.api_key, "abc");
    }

    #[test]
    fn errors_and_names_all_missing_required() {
        let err = load_str("DAIMONOS_AGENT_PROVIDER=openrouter\n").unwrap_err();
        for k in [
            "DAIMONOS_AGENT_MODEL",
            "DAIMONOS_AGENT_BASE_URL",
            "DAIMONOS_AGENT_APPROVAL_MODE",
            "DAIMONOS_AGENT_API_KEY",
        ] {
            assert!(err.contains(k), "error should name {k}: {err}");
        }
        assert!(!err.contains("DAIMONOS_AGENT_PROVIDER"), "present key not flagged: {err}");
    }

    #[test]
    fn empty_value_counts_as_missing() {
        let s = base().replace("DAIMONOS_AGENT_API_KEY=sk-test", "DAIMONOS_AGENT_API_KEY=   ");
        let err = load_str(&s).unwrap_err();
        assert!(err.contains("DAIMONOS_AGENT_API_KEY"), "{err}");
    }

    #[test]
    fn rejects_invalid_provider_and_approval_mode() {
        let s = base().replace("openrouter", "ollama");
        assert!(load_str(&s).unwrap_err().contains("unsupported"));
        let s = base().replace("APPROVAL_MODE=interactive", "APPROVAL_MODE=yolo");
        assert!(load_str(&s).unwrap_err().contains("invalid"));
    }

    #[test]
    fn parses_comma_separated_command_lists() {
        let s = base()
            + "DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file, search ,ls\n\
               DAIMONOS_AGENT_DENIED_COMMANDS=exec\n";
        let e = load_str(&s).unwrap();
        assert_eq!(e.allowed_commands, vec!["read_file", "search", "ls"]);
        assert_eq!(e.denied_commands, vec!["exec"]);
    }

    #[test]
    fn resolve_path_prefers_flag_then_env() {
        let flag = PathBuf::from("/tmp/x/agent.env");
        assert_eq!(AgentEnv::resolve_path(Some(flag.clone())), Some(flag));
    }

    #[test]
    fn to_safety_policy_maps_mode() {
        let mut e = load_str(&base()).unwrap();
        e.approval_mode = "auto".into();
        assert!(matches!(
            e.to_safety_policy(None).approval_mode,
            crate::safety::ApprovalMode::Auto
        ));
    }
}
