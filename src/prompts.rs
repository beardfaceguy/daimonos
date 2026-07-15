//! Model-facing prompts, externalized so they are editable at runtime without
//! recompiling (vikunja #974).
//!
//! Every prompt ships as an embedded default — the versioned file under
//! `prompts/`, pulled in at compile time via `include_str!` — and can be
//! overridden at runtime by pointing the matching `[prompts]` config key at a
//! file (see `config::PromptsConfig` and `daimonos.default.toml`). A
//! configured-but-unreadable override falls back to the embedded default with a
//! stderr warning, mirroring `config::load`'s warn-and-fall-back convention (a
//! bad prompt path should not take the process down).
//!
//! WARNING: these prompts steer agent behavior, tool-use strategy, and (for the
//! MCP `mcp_instructions` terse directive) output token cost. The whole file is
//! sent to the model verbatim — do not add comments inside a prompt file. Keep
//! guidance in `prompts/README.md` or in `daimonos.toml` comments instead.

use crate::compaction::CompactionPolicy;
use crate::config::Config;
use std::path::PathBuf;

/// Embedded default: core agent system prompt (`daimonos agent` / `chat` / ACP).
pub const AGENT_SYSTEM_DEFAULT: &str = include_str!("../prompts/agent_system.md");
/// Embedded default: static MCP server instructions (`daimonos --mcp`). The
/// dynamic workspace context (path, project type, dirs, Starlark signatures) is
/// appended by `mcp::build_instructions`, not stored here.
pub const MCP_INSTRUCTIONS_DEFAULT: &str = include_str!("../prompts/mcp_instructions.md");
/// Embedded default: KGL orientation hint (emitted only when KGL auto-index is on).
pub const KGL_HINT_DEFAULT: &str = include_str!("../prompts/kgl_hint.md");
/// Embedded default: compaction summarizer system prompt.
pub const SUMMARY_DEFAULT: &str = include_str!("../prompts/summary.md");

/// Expand a leading `~/` to `$HOME`. Mirrors the tilde handling in
/// `AnalyticsConfig::resolved_db_path`; kept local so this module has no
/// dependency beyond `std`.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Resolve one prompt: read the override file when the key is set and non-empty,
/// otherwise use the embedded default. A set-but-unreadable path warns and falls
/// back so a typo never silently swaps in the wrong prompt without a trace.
fn resolve(name: &str, override_path: Option<&str>, embedded: &str) -> String {
    match override_path {
        Some(p) if !p.trim().is_empty() => match std::fs::read_to_string(expand_tilde(p)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "daimonos: prompt override '{name}' ({p}) unreadable: {e}; using embedded default"
                );
                embedded.to_string()
            }
        },
        _ => embedded.to_string(),
    }
}

/// Core agent system prompt for the `agent`/`chat`/ACP runtimes.
pub fn agent_system(cfg: &Config) -> String {
    resolve(
        "agent_system",
        cfg.prompts.agent_system.as_deref(),
        AGENT_SYSTEM_DEFAULT,
    )
}

/// Static MCP server instructions (before dynamic workspace context is appended).
pub fn mcp_instructions(cfg: &Config) -> String {
    resolve(
        "mcp_instructions",
        cfg.prompts.mcp_instructions.as_deref(),
        MCP_INSTRUCTIONS_DEFAULT,
    )
}

/// KGL orientation hint text.
pub fn kgl_hint(cfg: &Config) -> String {
    resolve(
        "kgl_hint",
        cfg.prompts.kgl_hint.as_deref(),
        KGL_HINT_DEFAULT,
    )
}

/// Apply the `[prompts].summary` override to an already-resolved compaction
/// policy. This is the one prompt whose runtime override does not flow through
/// the TOML `Config` at its use site (compaction runs deep in `AgentSession`,
/// which has no `Config`), so it is injected here where `Config` is in scope.
///
/// Precedence (highest first): the agent-env `DAIMONOS_AGENT_SUMMARY_PROMPT`
/// (already parsed into `policy.summary_prompt`) > `[prompts].summary` >
/// embedded `summary.md` (the fallback in `compaction::default_summary_prompt`).
/// So this only fills in a policy that did not already get an env override.
pub fn apply_summary_override(
    compaction: Option<CompactionPolicy>,
    cfg: &Config,
) -> Option<CompactionPolicy> {
    let path = match cfg.prompts.summary.as_deref() {
        Some(p) if !p.trim().is_empty() => p,
        _ => return compaction,
    };
    compaction.map(|mut policy| {
        if policy.summary_prompt.is_none() {
            match std::fs::read_to_string(expand_tilde(path)) {
                Ok(s) => policy.summary_prompt = Some(s),
                Err(e) => eprintln!(
                    "daimonos: prompt override 'summary' ({path}) unreadable: {e}; using default"
                ),
            }
        }
        policy
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CompactionPolicy {
        CompactionPolicy {
            high_water: 0.75,
            low_water: 0.5,
            context_window: 100_000,
            output_reservation: 4_000,
            summary_model: None,
            summary_prompt: None,
        }
    }

    // --- embedded defaults ---

    #[test]
    fn agent_system_default_prefers_execute_script() {
        let p = AGENT_SYSTEM_DEFAULT;
        assert!(p.contains("execute_script"), "must mention execute_script");
        assert!(
            p.contains("round-trip") || p.contains("sequential"),
            "must explain the round-trip rationale"
        );
    }

    #[test]
    fn mcp_instructions_default_has_terse_directive() {
        assert!(MCP_INSTRUCTIONS_DEFAULT.contains("Terse output"));
        assert!(MCP_INSTRUCTIONS_DEFAULT.contains("execute_script"));
        assert!(MCP_INSTRUCTIONS_DEFAULT.contains("Use daimonos tools"));
    }

    #[test]
    fn kgl_hint_default_mentions_kgl_query() {
        assert!(KGL_HINT_DEFAULT.contains("kgl_query"));
    }

    #[test]
    fn summary_default_is_a_summarizer_prompt() {
        assert!(SUMMARY_DEFAULT.to_lowercase().contains("summar"));
    }

    #[test]
    fn defaults_are_non_empty() {
        for s in [
            AGENT_SYSTEM_DEFAULT,
            MCP_INSTRUCTIONS_DEFAULT,
            KGL_HINT_DEFAULT,
            SUMMARY_DEFAULT,
        ] {
            assert!(!s.trim().is_empty());
        }
    }

    // --- override resolution ---

    #[test]
    fn unset_key_uses_embedded_default() {
        let cfg = Config::default();
        assert_eq!(agent_system(&cfg), AGENT_SYSTEM_DEFAULT);
        assert_eq!(mcp_instructions(&cfg), MCP_INSTRUCTIONS_DEFAULT);
        assert_eq!(kgl_hint(&cfg), KGL_HINT_DEFAULT);
    }

    #[test]
    fn empty_override_path_uses_default() {
        let mut cfg = Config::default();
        cfg.prompts.agent_system = Some("   ".to_string());
        assert_eq!(agent_system(&cfg), AGENT_SYSTEM_DEFAULT);
    }

    #[test]
    fn override_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.md");
        std::fs::write(&path, "CUSTOM AGENT PROMPT").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.agent_system = Some(path.to_string_lossy().to_string());
        assert_eq!(agent_system(&cfg), "CUSTOM AGENT PROMPT");
    }

    #[test]
    fn unreadable_override_falls_back_to_default() {
        let mut cfg = Config::default();
        cfg.prompts.mcp_instructions = Some("/definitely/not/a/real/prompt.md".to_string());
        assert_eq!(mcp_instructions(&cfg), MCP_INSTRUCTIONS_DEFAULT);
    }

    // --- summary override injection ---

    #[test]
    fn summary_override_unset_leaves_policy_untouched() {
        let cfg = Config::default();
        let out = apply_summary_override(Some(policy()), &cfg).unwrap();
        assert_eq!(out.summary_prompt, None);
    }

    #[test]
    fn summary_override_fills_empty_policy_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sum.md");
        std::fs::write(&path, "CUSTOM SUMMARY").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.summary = Some(path.to_string_lossy().to_string());
        let out = apply_summary_override(Some(policy()), &cfg).unwrap();
        assert_eq!(out.summary_prompt.as_deref(), Some("CUSTOM SUMMARY"));
    }

    #[test]
    fn summary_override_does_not_clobber_env_value() {
        // An agent-env DAIMONOS_AGENT_SUMMARY_PROMPT has already populated
        // policy.summary_prompt; the config path must not override it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sum.md");
        std::fs::write(&path, "CONFIG SUMMARY").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.summary = Some(path.to_string_lossy().to_string());
        let mut p = policy();
        p.summary_prompt = Some("ENV SUMMARY".to_string());
        let out = apply_summary_override(Some(p), &cfg).unwrap();
        assert_eq!(out.summary_prompt.as_deref(), Some("ENV SUMMARY"));
    }

    #[test]
    fn summary_override_on_disabled_compaction_is_none() {
        let mut cfg = Config::default();
        cfg.prompts.summary = Some("/whatever.md".to_string());
        assert!(apply_summary_override(None, &cfg).is_none());
    }
}
