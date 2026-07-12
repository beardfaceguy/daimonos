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

use crate::compaction::CompactionPolicy;
use crate::providers::LlmProvider;

/// Compaction settings parsed from the env file when `DAIMONOS_AGENT_CONTEXT_WINDOW`
/// is omitted (vikunja #965): everything a [`CompactionPolicy`] needs except
/// the window itself, which is resolved live from the provider in `main.rs`
/// once the effective (possibly `--model`-overridden) model is known.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionSpec {
    pub high_water: f64,
    pub low_water: f64,
    pub output_reservation: u64,
    pub summary_model: Option<String>,
    pub summary_prompt: Option<String>,
}

/// Parsed compaction configuration (ADR-002 amendment, vikunja #965). Keeps
/// [`CompactionPolicy`] — and therefore `compaction.rs`/`agent.rs` — untouched
/// by isolating the "window not yet known" state here at the config layer.
///
/// - `Off`: `DAIMONOS_AGENT_COMPACTION=off`.
/// - `Ready`: `on` with `DAIMONOS_AGENT_CONTEXT_WINDOW` present — validated in
///   full at parse time, byte-for-byte as before this change.
/// - `NeedsWindow`: `on` with the window omitted — the window (and its
///   dependent validation) is deferred to [`AgentEnv::resolve_compaction`].
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionConfig {
    Off,
    Ready(CompactionPolicy),
    NeedsWindow(CompactionSpec),
}

/// Required scalar keys — `daimonos agent` refuses to run if any is absent/empty.
const REQUIRED: &[&str] = &[
    "DAIMONOS_AGENT_PROVIDER",
    "DAIMONOS_AGENT_MODEL",
    "DAIMONOS_AGENT_BASE_URL",
    "DAIMONOS_AGENT_APPROVAL_MODE",
    "DAIMONOS_AGENT_API_KEY",
    // ADR-002: compaction must be an explicit choice — no default in code.
    "DAIMONOS_AGENT_COMPACTION",
];

/// Numeric keys required when `DAIMONOS_AGENT_COMPACTION=on` (ADR-002: no
/// values in code — the budget math comes entirely from the env file).
/// `DAIMONOS_AGENT_CONTEXT_WINDOW` is deliberately NOT here: it is optional
/// (vikunja #965) and resolved from the provider when omitted.
const COMPACTION_REQUIRED: &[&str] = &[
    "DAIMONOS_AGENT_COMPACTION_HIGH_WATER",
    "DAIMONOS_AGENT_COMPACTION_LOW_WATER",
    "DAIMONOS_AGENT_OUTPUT_RESERVATION",
];

/// Validated agent connection config.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEnv {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub approval_mode: String,
    pub api_key: String,
    pub allowed_commands: Vec<String>,
    pub denied_commands: Vec<String>,
    /// Candidate models for the ACP model picker (vikunja #960), from the
    /// optional `DAIMONOS_AGENT_MODELS` comma-list. `model` (the active one)
    /// is always present — prepended if the list omits it, and the list is
    /// deduped preserving first-seen order. Never empty (at minimum `[model]`).
    pub models: Vec<String>,
    /// Context/window compaction (ADR-002 + #965 amendment). Resolved into an
    /// effective `Option<CompactionPolicy>` by [`Self::resolve_compaction`]
    /// once the provider and effective model are known.
    pub compaction: CompactionConfig,
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

        let model = present("DAIMONOS_AGENT_MODEL").unwrap();
        let models = candidate_models(&model, parse_list(vars.get("DAIMONOS_AGENT_MODELS")));
        let compaction = parse_compaction(vars, path)?;

        Ok(AgentEnv {
            provider,
            model,
            base_url: present("DAIMONOS_AGENT_BASE_URL").unwrap(),
            approval_mode,
            api_key: present("DAIMONOS_AGENT_API_KEY").unwrap(),
            allowed_commands: parse_list(vars.get("DAIMONOS_AGENT_ALLOWED_COMMANDS")),
            denied_commands: parse_list(vars.get("DAIMONOS_AGENT_DENIED_COMMANDS")),
            models,
            compaction,
        })
    }

    /// Resolve the parsed [`CompactionConfig`] into an effective
    /// `Option<CompactionPolicy>` for the frontends (ADR-002 amendment,
    /// vikunja #965). Must be called from `main.rs` *after* the provider is
    /// built and the effective (possibly `--model`-overridden) model is
    /// known, so a `NeedsWindow` config queries the provider for the model
    /// actually in use.
    ///
    /// - `Off` → `None` (compaction disabled).
    /// - `Ready(policy)` → `Some(policy)`, unchanged.
    /// - `NeedsWindow(spec)` → query `provider.context_window(effective_model)`
    ///   and apply the deferred window checks. A lookup failure is a HARD
    ///   ERROR (never a silent fallback to compaction-off) naming the model
    ///   and directing the user to set `DAIMONOS_AGENT_CONTEXT_WINDOW`.
    pub async fn resolve_compaction(
        &self,
        provider: &dyn LlmProvider,
        effective_model: &str,
    ) -> Result<Option<CompactionPolicy>, String> {
        match &self.compaction {
            CompactionConfig::Off => Ok(None),
            CompactionConfig::Ready(policy) => Ok(Some(policy.clone())),
            CompactionConfig::NeedsWindow(spec) => {
                let context_window = provider
                    .context_window(effective_model)
                    .await
                    .filter(|&w| w > 0)
                    .ok_or_else(|| {
                        format!(
                            "could not determine the context window for model '{effective_model}' \
                         from the provider (network error, unknown model id, or the provider \
                         does not report it). Set DAIMONOS_AGENT_CONTEXT_WINDOW explicitly in \
                         the agent env file, or set DAIMONOS_AGENT_COMPACTION=off."
                        )
                    })?;
                if spec.output_reservation >= context_window {
                    return Err(format!(
                        "DAIMONOS_AGENT_OUTPUT_RESERVATION ({}) must be smaller than the \
                         provider-reported context window ({context_window}) for model \
                         '{effective_model}'",
                        spec.output_reservation
                    ));
                }
                Ok(Some(CompactionPolicy {
                    high_water: spec.high_water,
                    low_water: spec.low_water,
                    context_window,
                    output_reservation: spec.output_reservation,
                    summary_model: spec.summary_model.clone(),
                    summary_prompt: spec.summary_prompt.clone(),
                }))
            }
        }
    }

    /// Global path for persisted "always" (Y) approvals, fixed at
    /// ~/.config/daimonos/agent-approvals (independent of the agent env file).
    pub fn approvals_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("daimonos")
                .join("agent-approvals")
        })
    }

    /// Build a `SafetyPolicy` from the approval mode + allow/deny lists, seeding
    /// the auto-approve set from the persisted approvals file so previously
    /// "always"-approved tools skip the prompt.
    pub fn to_safety_policy(
        &self,
        approve_fn: Option<crate::safety::ApproveFn>,
    ) -> crate::safety::SafetyPolicy {
        let approval_mode = match self.approval_mode.as_str() {
            "auto" => crate::safety::ApprovalMode::Auto,
            "paranoid" => crate::safety::ApprovalMode::Paranoid,
            _ => crate::safety::ApprovalMode::Interactive,
        };
        let approvals_path = Self::approvals_path();
        let seed = approvals_path
            .as_ref()
            .map(|p| crate::safety::load_approvals(p))
            .unwrap_or_default();
        crate::safety::SafetyPolicy {
            approval_mode,
            allowed_commands: self.allowed_commands.clone(),
            denied_commands: self.denied_commands.clone(),
            approve_fn,
            auto_approve: std::sync::Arc::new(std::sync::Mutex::new(seed)),
            approvals_path,
        }
    }
}

/// Parse the ADR-002 compaction keys. `DAIMONOS_AGENT_COMPACTION` itself is
/// in [`REQUIRED`] (checked by the caller); `on` additionally requires every
/// [`COMPACTION_REQUIRED`] numeric key — there are deliberately no default
/// values in code. `DAIMONOS_AGENT_CONTEXT_WINDOW` is optional (vikunja
/// #965): present → [`CompactionConfig::Ready`] (fully validated here);
/// absent → [`CompactionConfig::NeedsWindow`] (window + its dependent checks
/// deferred to [`AgentEnv::resolve_compaction`]). The window-independent
/// watermark rule `0 < low_water < high_water < 1` is always enforced here.
fn parse_compaction(
    vars: &HashMap<String, String>,
    path: &Path,
) -> Result<CompactionConfig, String> {
    let present = |k: &str| {
        vars.get(k)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
    };
    // Caller has verified presence (REQUIRED).
    let switch = present("DAIMONOS_AGENT_COMPACTION").unwrap();
    match switch.as_str() {
        "off" => Ok(CompactionConfig::Off),
        "on" => {
            let missing: Vec<&str> = COMPACTION_REQUIRED
                .iter()
                .copied()
                .filter(|k| present(k).is_none())
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "agent env file {}: DAIMONOS_AGENT_COMPACTION=on also requires: {}",
                    path.display(),
                    missing.join(", ")
                ));
            }
            let num = |k: &str| -> Result<f64, String> {
                let raw = present(k).unwrap();
                raw.parse::<f64>().map_err(|_| {
                    format!(
                        "agent env file {}: {k} '{raw}' is not a number",
                        path.display()
                    )
                })
            };
            let int = |k: &str| -> Result<u64, String> {
                let raw = present(k).unwrap();
                raw.parse::<u64>().map_err(|_| {
                    format!(
                        "agent env file {}: {k} '{raw}' is not a whole number of tokens",
                        path.display()
                    )
                })
            };
            let high_water = num("DAIMONOS_AGENT_COMPACTION_HIGH_WATER")?;
            let low_water = num("DAIMONOS_AGENT_COMPACTION_LOW_WATER")?;
            let output_reservation = int("DAIMONOS_AGENT_OUTPUT_RESERVATION")?;

            if !(low_water > 0.0 && low_water < high_water && high_water < 1.0) {
                return Err(format!(
                    "agent env file {}: compaction watermarks must satisfy 0 < LOW_WATER < HIGH_WATER < 1 (got low={low_water}, high={high_water})",
                    path.display()
                ));
            }

            // Optional: unset → the session's main model / built-in prompt
            // template (referential fallbacks, not magic numbers).
            let summary_model = present("DAIMONOS_AGENT_SUMMARY_MODEL");
            let summary_prompt = present("DAIMONOS_AGENT_SUMMARY_PROMPT");

            match present("DAIMONOS_AGENT_CONTEXT_WINDOW") {
                Some(_) => {
                    let context_window = int("DAIMONOS_AGENT_CONTEXT_WINDOW")?;
                    if context_window == 0 {
                        return Err(format!(
                            "agent env file {}: DAIMONOS_AGENT_CONTEXT_WINDOW must be > 0",
                            path.display()
                        ));
                    }
                    if output_reservation >= context_window {
                        return Err(format!(
                            "agent env file {}: DAIMONOS_AGENT_OUTPUT_RESERVATION ({output_reservation}) must be smaller than DAIMONOS_AGENT_CONTEXT_WINDOW ({context_window})",
                            path.display()
                        ));
                    }
                    Ok(CompactionConfig::Ready(CompactionPolicy {
                        high_water,
                        low_water,
                        context_window,
                        output_reservation,
                        summary_model,
                        summary_prompt,
                    }))
                }
                None => Ok(CompactionConfig::NeedsWindow(CompactionSpec {
                    high_water,
                    low_water,
                    output_reservation,
                    summary_model,
                    summary_prompt,
                })),
            }
        }
        other => Err(format!(
            "agent env file {}: DAIMONOS_AGENT_COMPACTION '{other}' invalid (valid: on, off)",
            path.display()
        )),
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

/// Build the picker's candidate model list: `current` is always present
/// (prepended if the configured list omits it), duplicates removed while
/// preserving first-seen order. Result is never empty.
fn candidate_models(current: &str, configured: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |m: String| {
        if !out.contains(&m) {
            out.push(m);
        }
    };
    if !configured.iter().any(|m| m == current) {
        push_unique(current.to_string());
    }
    for m in configured {
        push_unique(m);
    }
    out
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
         DAIMONOS_AGENT_API_KEY=sk-test\n\
         DAIMONOS_AGENT_COMPACTION=off\n"
            .to_string()
    }

    fn compaction_on() -> String {
        base().replace(
            "DAIMONOS_AGENT_COMPACTION=off",
            "DAIMONOS_AGENT_COMPACTION=on",
        ) + "DAIMONOS_AGENT_COMPACTION_HIGH_WATER=0.75\n\
               DAIMONOS_AGENT_COMPACTION_LOW_WATER=0.5\n\
               DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n\
               DAIMONOS_AGENT_OUTPUT_RESERVATION=8192\n"
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
        // No DAIMONOS_AGENT_MODELS → picker list is just the active model.
        assert_eq!(e.models, vec!["anthropic/claude-sonnet-4.6"]);
    }

    #[test]
    fn models_list_defaults_to_just_the_active_model() {
        let e = load_str(&base()).unwrap();
        assert_eq!(e.models, vec![e.model.clone()]);
    }

    #[test]
    fn models_list_includes_configured_and_keeps_order() {
        let s = base()
            + "DAIMONOS_AGENT_MODELS=anthropic/claude-sonnet-4.6, anthropic/claude-opus-4.1 ,anthropic/claude-haiku-4.5\n";
        let e = load_str(&s).unwrap();
        assert_eq!(
            e.models,
            vec![
                "anthropic/claude-sonnet-4.6",
                "anthropic/claude-opus-4.1",
                "anthropic/claude-haiku-4.5",
            ]
        );
    }

    #[test]
    fn active_model_prepended_when_absent_from_configured_list() {
        let s =
            base() + "DAIMONOS_AGENT_MODELS=anthropic/claude-opus-4.1,anthropic/claude-haiku-4.5\n";
        let e = load_str(&s).unwrap();
        // active model (sonnet-4.6) not in the configured list → prepended.
        assert_eq!(
            e.models,
            vec![
                "anthropic/claude-sonnet-4.6",
                "anthropic/claude-opus-4.1",
                "anthropic/claude-haiku-4.5",
            ]
        );
    }

    #[test]
    fn models_list_dedupes_preserving_first_seen() {
        let s = base()
            + "DAIMONOS_AGENT_MODELS=anthropic/claude-opus-4.1,anthropic/claude-sonnet-4.6,anthropic/claude-opus-4.1\n";
        let e = load_str(&s).unwrap();
        assert_eq!(
            e.models,
            vec!["anthropic/claude-opus-4.1", "anthropic/claude-sonnet-4.6"]
        );
    }

    #[test]
    fn dotenv_handles_comments_quotes_export_blanks() {
        let s = "# a comment\n\n  export DAIMONOS_AGENT_PROVIDER = \"anthropic\" \n\
                 DAIMONOS_AGENT_MODEL='claude-opus-4-8'\n\
                 DAIMONOS_AGENT_BASE_URL=https://api.anthropic.com\n\
                 DAIMONOS_AGENT_APPROVAL_MODE=auto\n\
                 DAIMONOS_AGENT_API_KEY=abc\n\
                 DAIMONOS_AGENT_COMPACTION=off\n";
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
        assert!(
            !err.contains("DAIMONOS_AGENT_PROVIDER"),
            "present key not flagged: {err}"
        );
    }

    #[test]
    fn empty_value_counts_as_missing() {
        let s = base().replace(
            "DAIMONOS_AGENT_API_KEY=sk-test",
            "DAIMONOS_AGENT_API_KEY=   ",
        );
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

    // --- compaction config (ADR-002, vikunja #962) ---

    #[test]
    fn compaction_off_parses_to_none() {
        assert_eq!(load_str(&base()).unwrap().compaction, CompactionConfig::Off);
    }

    #[test]
    fn compaction_key_is_required() {
        let s = base().replace("DAIMONOS_AGENT_COMPACTION=off\n", "");
        let err = load_str(&s).unwrap_err();
        assert!(err.contains("DAIMONOS_AGENT_COMPACTION"), "{err}");
    }

    #[test]
    fn compaction_invalid_switch_value_errors() {
        let s = base().replace("COMPACTION=off", "COMPACTION=maybe");
        let err = load_str(&s).unwrap_err();
        assert!(err.contains("invalid") && err.contains("maybe"), "{err}");
    }

    fn ready_policy(env: &AgentEnv) -> CompactionPolicy {
        match &env.compaction {
            CompactionConfig::Ready(p) => p.clone(),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn compaction_on_parses_full_policy() {
        let p = ready_policy(&load_str(&compaction_on()).unwrap());
        assert_eq!(p.high_water, 0.75);
        assert_eq!(p.low_water, 0.5);
        assert_eq!(p.context_window, 200_000);
        assert_eq!(p.output_reservation, 8192);
        assert_eq!(p.summary_model, None);
        assert_eq!(p.summary_prompt, None);
        assert_eq!(p.budget(), 191_808);
    }

    #[test]
    fn compaction_on_requires_remaining_numeric_keys_and_names_missing() {
        // CONTEXT_WINDOW is now optional (#965) — removing it alone must NOT
        // error; the still-required numeric keys are reported when absent.
        let s = compaction_on()
            .replace("DAIMONOS_AGENT_COMPACTION_LOW_WATER=0.5\n", "")
            .replace("DAIMONOS_AGENT_OUTPUT_RESERVATION=8192\n", "");
        let err = load_str(&s).unwrap_err();
        assert!(err.contains("DAIMONOS_AGENT_COMPACTION_LOW_WATER"), "{err}");
        assert!(err.contains("DAIMONOS_AGENT_OUTPUT_RESERVATION"), "{err}");
        assert!(
            !err.contains("HIGH_WATER"),
            "present key must not be flagged: {err}"
        );
        assert!(
            !err.contains("CONTEXT_WINDOW"),
            "optional key must not be flagged: {err}"
        );
        assert!(
            err.contains("<test>"),
            "error must name the file path: {err}"
        );
    }

    #[test]
    fn compaction_off_ignores_missing_numeric_keys() {
        // off → the numeric knobs are not required.
        assert_eq!(load_str(&base()).unwrap().compaction, CompactionConfig::Off);
    }

    // --- optional context window (#965) ---

    #[test]
    fn compaction_on_without_window_parses_to_needs_window() {
        let s = compaction_on().replace("DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n", "");
        let spec = match load_str(&s).unwrap().compaction {
            CompactionConfig::NeedsWindow(spec) => spec,
            other => panic!("expected NeedsWindow, got {other:?}"),
        };
        assert_eq!(spec.high_water, 0.75);
        assert_eq!(spec.low_water, 0.5);
        assert_eq!(spec.output_reservation, 8192);
    }

    #[test]
    fn compaction_needs_window_still_enforces_watermarks_at_parse() {
        // Window-independent validation must fire even when the window is
        // deferred to the provider.
        let s = compaction_on()
            .replace("DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n", "")
            .replace("HIGH_WATER=0.75", "HIGH_WATER=0.4"); // low(0.5) > high(0.4)
        let err = load_str(&s).unwrap_err();
        assert!(err.contains("0 < LOW_WATER < HIGH_WATER < 1"), "{err}");
    }

    #[test]
    fn compaction_needs_window_captures_summary_keys() {
        let s = compaction_on().replace("DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n", "")
            + "DAIMONOS_AGENT_SUMMARY_MODEL=anthropic/claude-haiku-4.5\n";
        match load_str(&s).unwrap().compaction {
            CompactionConfig::NeedsWindow(spec) => {
                assert_eq!(
                    spec.summary_model.as_deref(),
                    Some("anthropic/claude-haiku-4.5")
                );
            }
            other => panic!("expected NeedsWindow, got {other:?}"),
        }
    }

    // A provider whose only interesting behavior is what context_window()
    // reports, for resolve_compaction() tests.
    struct FakeProvider(Option<u64>);

    #[async_trait::async_trait]
    impl LlmProvider for FakeProvider {
        async fn complete(
            &self,
            _ctx: &crate::providers::Context,
            _opts: &crate::providers::CompleteOpts,
        ) -> crate::providers::LlmResponse {
            crate::providers::LlmResponse::error("unused")
        }
        async fn context_window(&self, _model: &str) -> Option<u64> {
            self.0
        }
    }

    #[tokio::test]
    async fn resolve_off_yields_none() {
        let env = load_str(&base()).unwrap();
        let out = env
            .resolve_compaction(&FakeProvider(Some(200_000)), "m")
            .await
            .unwrap();
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn resolve_ready_returns_policy_unchanged_without_querying() {
        let env = load_str(&compaction_on()).unwrap();
        // FakeProvider(None) would fail a lookup — proving Ready never queries.
        let out = env
            .resolve_compaction(&FakeProvider(None), "m")
            .await
            .unwrap();
        assert_eq!(out.unwrap().context_window, 200_000);
    }

    #[tokio::test]
    async fn resolve_needs_window_uses_provider_value() {
        let s = compaction_on().replace("DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n", "");
        let env = load_str(&s).unwrap();
        let policy = env
            .resolve_compaction(&FakeProvider(Some(128_000)), "some/model")
            .await
            .unwrap()
            .expect("policy");
        assert_eq!(policy.context_window, 128_000);
        assert_eq!(policy.output_reservation, 8192);
        assert_eq!(policy.high_water, 0.75);
        assert_eq!(policy.budget(), 128_000 - 8192);
    }

    #[tokio::test]
    async fn resolve_needs_window_hard_errors_on_lookup_failure() {
        let s = compaction_on().replace("DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n", "");
        let env = load_str(&s).unwrap();
        let err = env
            .resolve_compaction(&FakeProvider(None), "acme/model-x")
            .await
            .unwrap_err();
        assert!(err.contains("acme/model-x"), "error names the model: {err}");
        assert!(
            err.contains("DAIMONOS_AGENT_CONTEXT_WINDOW"),
            "error names the key: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_needs_window_rejects_reservation_at_or_above_window() {
        let s = compaction_on().replace("DAIMONOS_AGENT_CONTEXT_WINDOW=200000\n", "");
        let env = load_str(&s).unwrap();
        // Provider reports a window smaller than the 8192 reservation.
        let err = env
            .resolve_compaction(&FakeProvider(Some(4096)), "tiny/model")
            .await
            .unwrap_err();
        assert!(err.contains("must be smaller"), "{err}");
    }

    #[test]
    fn compaction_rejects_bad_watermark_ordering() {
        for (low, high) in [
            ("0.8", "0.75"),
            ("0.75", "0.75"),
            ("0.0", "0.75"),
            ("0.5", "1.0"),
        ] {
            let s = compaction_on()
                .replace("HIGH_WATER=0.75", &format!("HIGH_WATER={high}"))
                .replace("LOW_WATER=0.5", &format!("LOW_WATER={low}"));
            let err = load_str(&s).unwrap_err();
            assert!(
                err.contains("0 < LOW_WATER < HIGH_WATER < 1"),
                "low={low} high={high}: {err}"
            );
        }
    }

    #[test]
    fn compaction_rejects_non_numeric_values() {
        let s = compaction_on().replace("HIGH_WATER=0.75", "HIGH_WATER=lots");
        assert!(load_str(&s).unwrap_err().contains("not a number"));
        let s = compaction_on().replace("CONTEXT_WINDOW=200000", "CONTEXT_WINDOW=2e5");
        assert!(load_str(&s).unwrap_err().contains("not a whole number"));
    }

    #[test]
    fn compaction_rejects_reservation_at_or_above_window() {
        let s = compaction_on().replace("OUTPUT_RESERVATION=8192", "OUTPUT_RESERVATION=200000");
        assert!(load_str(&s).unwrap_err().contains("must be smaller"));
        let s = compaction_on().replace("CONTEXT_WINDOW=200000", "CONTEXT_WINDOW=0");
        // window 0 → the window>0 check fires (reservation check would too).
        assert!(load_str(&s).is_err());
    }

    #[test]
    fn compaction_optional_summary_keys_are_captured() {
        let s = compaction_on()
            + "DAIMONOS_AGENT_SUMMARY_MODEL=anthropic/claude-haiku-4.5\n\
               DAIMONOS_AGENT_SUMMARY_PROMPT=Summarize tersely.\n";
        let p = ready_policy(&load_str(&s).unwrap());
        assert_eq!(
            p.summary_model.as_deref(),
            Some("anthropic/claude-haiku-4.5")
        );
        assert_eq!(p.summary_prompt.as_deref(), Some("Summarize tersely."));
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
