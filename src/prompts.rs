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
use std::io::Write;
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

/// Canonical prompt keys, in a stable display order. This is the single list
/// used by `default_by_name`, the `--print-prompt` flag, and the `--dump-prompts`
/// scaffold, so adding a prompt means editing here and `default_by_name` only.
pub const PROMPT_NAMES: [&str; 5] = [
    "agent_system",
    "mcp_instructions",
    "kgl_hint",
    "summary",
    "tool_descriptions",
];

/// Embedded baseline default for a prompt key, or `None` for an unknown key.
/// Backs `--print-prompt` and `--dump-prompts` so binary-only users can recover
/// the baseline they are overriding against (vikunja #980).
pub fn default_by_name(name: &str) -> Option<&'static str> {
    match name {
        "agent_system" => Some(AGENT_SYSTEM_DEFAULT),
        "mcp_instructions" => Some(MCP_INSTRUCTIONS_DEFAULT),
        "kgl_hint" => Some(KGL_HINT_DEFAULT),
        "summary" => Some(SUMMARY_DEFAULT),
        "tool_descriptions" => Some(crate::tool_descriptions::DEFAULT_TEXT),
        _ => None,
    }
}

pub fn prompt_filename(name: &str) -> String {
    let extension = if name == "tool_descriptions" {
        "toml"
    } else {
        "md"
    };
    format!("{name}.{extension}")
}

/// Default scaffold directory for `--dump-prompts`: `<config_home>/daimonos/prompts`,
/// aligned with where `config.toml` is discovered.
pub fn default_prompts_dir() -> Option<PathBuf> {
    crate::config::dirs_next().map(|d| d.join("daimonos").join("prompts"))
}

/// Default optional user-instructions file for the agent runtimes. This follows
/// the same config-home resolution as `config.toml`: `$XDG_CONFIG_HOME` when
/// set, otherwise `~/.config`.
pub fn default_agent_instructions_path() -> Option<PathBuf> {
    crate::config::dirs_next().map(|d| d.join("daimonos").join("agent-instructions.md"))
}

/// Load additional instructions for `agent`, `chat`, and ACP. An explicit CLI
/// path must be readable. With no CLI override, a missing default file is the
/// normal "no extra rules" case; any other read error is surfaced so an
/// existing rules file is never silently ignored.
pub async fn load_agent_instructions(
    cli_path: Option<&std::path::Path>,
) -> std::io::Result<Option<String>> {
    let (path, missing_is_ok) = match cli_path {
        Some(path) => (path.to_path_buf(), false),
        None => match default_agent_instructions_path() {
            Some(path) => (path, true),
            None => return Ok(None),
        },
    };
    read_agent_instructions(&path, missing_is_ok).await
}

async fn read_agent_instructions(
    path: &std::path::Path,
    missing_is_ok: bool,
) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if missing_is_ok && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(std::io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", path.display()),
        )),
    }
}

/// Outcome of scaffolding the baseline prompts to disk.
pub struct DumpReport {
    pub dir: PathBuf,
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

/// Write every baseline prompt to `<dir>/<name>.md`. An empty/`None` `dir`
/// resolves to `default_prompts_dir()`. Existing files are left untouched unless
/// `force` is set (reported in `skipped`), so a re-run never clobbers a user's
/// edited copy by accident.
pub fn dump_defaults(dir: Option<&str>, force: bool) -> std::io::Result<DumpReport> {
    let dir = match dir {
        Some(d) if !d.trim().is_empty() => crate::paths::expand_tilde(d),
        _ => default_prompts_dir().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot resolve default prompts dir ($HOME/$XDG_CONFIG_HOME unset); pass a directory",
            )
        })?,
    };
    std::fs::create_dir_all(&dir)?;
    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for name in PROMPT_NAMES {
        let path = dir.join(prompt_filename(name));
        let content = default_by_name(name)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("no embedded default registered for prompt '{name}'"),
                )
            })?
            .as_bytes();
        if force {
            std::fs::write(&path, content)?;
        } else {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => file.write_all(content)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    skipped.push(name.to_string());
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        written.push(name.to_string());
    }
    Ok(DumpReport {
        dir,
        written,
        skipped,
    })
}

/// A ready-to-paste `[prompts]` TOML block pointing every key at `<dir>/<name>.md`,
/// for `--dump-prompts` to print after scaffolding.
pub fn prompts_toml_block(dir: &std::path::Path) -> String {
    let width = PROMPT_NAMES.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut out = String::from("[prompts]\n");
    for name in PROMPT_NAMES {
        let path = dir.join(prompt_filename(name));
        out.push_str(&format!(
            "{name:<width$} = {:?}\n",
            path.to_string_lossy(),
            width = width
        ));
    }
    out
}

/// Expand a leading `~/` to `$HOME`. Mirrors the tilde handling in
/// `AnalyticsConfig::resolved_db_path`; kept local so this module has no
/// dependency beyond `std`.
/// Resolve one prompt: read the override file when the key is set and non-empty,
/// otherwise use the embedded default. A set-but-unreadable path warns and falls
/// back so a typo never silently swaps in the wrong prompt without a trace.
async fn resolve(name: &str, override_path: Option<&str>, embedded: &str) -> String {
    match override_path {
        Some(p) if !p.trim().is_empty() => {
            match tokio::fs::read_to_string(crate::paths::expand_tilde(p)).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                    "daimonos: prompt override '{name}' ({p}) unreadable: {e}; using embedded default"
                );
                    embedded.to_string()
                }
            }
        }
        _ => embedded.to_string(),
    }
}

/// Core agent system prompt for the `agent`/`chat`/ACP runtimes, followed by
/// optional user instructions loaded during startup. The additional file is
/// appended verbatim with only a blank-line separator — no hidden instruction
/// text is injected around it.
pub async fn agent_system(cfg: &Config) -> String {
    let mut prompt = resolve(
        "agent_system",
        cfg.prompts.agent_system.as_deref(),
        AGENT_SYSTEM_DEFAULT,
    )
    .await;
    let Some(additional) = cfg.prompts.additional_agent_instructions.as_deref() else {
        return prompt;
    };
    if additional.is_empty() {
        return prompt;
    }
    if !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    if !prompt.ends_with("\n\n") {
        prompt.push('\n');
    }
    prompt.push_str(additional);
    prompt
}

/// Static MCP server instructions (before dynamic workspace context is appended).
pub async fn mcp_instructions(cfg: &Config) -> String {
    resolve(
        "mcp_instructions",
        cfg.prompts.mcp_instructions.as_deref(),
        MCP_INSTRUCTIONS_DEFAULT,
    )
    .await
}

/// KGL orientation hint text.
pub async fn kgl_hint(cfg: &Config) -> String {
    resolve(
        "kgl_hint",
        cfg.prompts.kgl_hint.as_deref(),
        KGL_HINT_DEFAULT,
    )
    .await
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
pub async fn apply_summary_override(
    compaction: Option<CompactionPolicy>,
    cfg: &Config,
) -> Option<CompactionPolicy> {
    let path = match cfg.prompts.summary.as_deref() {
        Some(p) if !p.trim().is_empty() => p,
        _ => return compaction,
    };
    let mut policy = compaction?;
    if policy.summary_prompt.is_none() {
        match tokio::fs::read_to_string(crate::paths::expand_tilde(path)).await {
            Ok(s) => policy.summary_prompt = Some(s),
            Err(e) => eprintln!(
                "daimonos: prompt override 'summary' ({path}) unreadable: {e}; using default"
            ),
        }
    }
    Some(policy)
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
    fn mcp_instructions_explain_starlark_top_level_loop_restriction() {
        let instructions = MCP_INSTRUCTIONS_DEFAULT.to_lowercase();
        assert!(
            instructions.contains("top-level") && instructions.contains("for"),
            "must explain that Starlark rejects top-level for loops"
        );
        assert!(
            instructions.contains("wrap loops") && instructions.contains("result = main()"),
            "must show the function-wrapper pattern for loops"
        );
    }

    #[test]
    fn prompts_guide_context_offload_into_execute_script() {
        // vikunja #1047 (RLM LID): keep large outputs in-sandbox and return a
        // compact `result` rather than flooding the root context.
        let agent = AGENT_SYSTEM_DEFAULT.to_lowercase();
        assert!(
            agent.contains("offload"),
            "agent system prompt must guide offloading large data"
        );
        assert!(
            agent.contains("compact"),
            "agent system prompt must ask for a compact result"
        );
        assert!(
            MCP_INSTRUCTIONS_DEFAULT
                .to_lowercase()
                .contains("large outputs"),
            "MCP instructions must guide large-output offloading"
        );
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

    #[tokio::test]
    async fn unset_key_uses_embedded_default() {
        let cfg = Config::default();
        assert_eq!(agent_system(&cfg).await, AGENT_SYSTEM_DEFAULT);
        assert_eq!(mcp_instructions(&cfg).await, MCP_INSTRUCTIONS_DEFAULT);
        assert_eq!(kgl_hint(&cfg).await, KGL_HINT_DEFAULT);
    }

    #[tokio::test]
    async fn empty_override_path_uses_default() {
        let mut cfg = Config::default();
        cfg.prompts.agent_system = Some("   ".to_string());
        assert_eq!(agent_system(&cfg).await, AGENT_SYSTEM_DEFAULT);
    }

    #[tokio::test]
    async fn override_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.md");
        std::fs::write(&path, "CUSTOM AGENT PROMPT").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.agent_system = Some(path.to_string_lossy().to_string());
        assert_eq!(agent_system(&cfg).await, "CUSTOM AGENT PROMPT");
    }

    #[tokio::test]
    async fn additional_agent_instructions_append_to_resolved_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.md");
        std::fs::write(&path, "CUSTOM AGENT PROMPT").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.agent_system = Some(path.to_string_lossy().to_string());
        cfg.prompts.additional_agent_instructions = Some("USER RULES\nverbatim\n".to_string());

        assert_eq!(
            agent_system(&cfg).await,
            "CUSTOM AGENT PROMPT\n\nUSER RULES\nverbatim\n"
        );
    }

    #[tokio::test]
    async fn empty_additional_agent_instructions_do_not_change_prompt() {
        let mut cfg = Config::default();
        cfg.prompts.additional_agent_instructions = Some(String::new());
        assert_eq!(agent_system(&cfg).await, AGENT_SYSTEM_DEFAULT);
    }

    #[tokio::test]
    async fn explicit_agent_instructions_file_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.md");
        std::fs::write(&path, "MY RULES").unwrap();

        assert_eq!(
            load_agent_instructions(Some(&path))
                .await
                .unwrap()
                .as_deref(),
            Some("MY RULES")
        );
    }

    #[tokio::test]
    async fn missing_explicit_agent_instructions_file_errors() {
        let path = std::path::Path::new("/definitely/not/agent-instructions.md");
        let err = load_agent_instructions(Some(path)).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains(path.to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn missing_default_agent_instructions_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-default.md");
        assert_eq!(read_agent_instructions(&path, true).await.unwrap(), None);
    }

    #[test]
    fn default_agent_instructions_path_uses_daimonos_config_dir() {
        if let Some(path) = default_agent_instructions_path() {
            assert!(path.ends_with("daimonos/agent-instructions.md"));
        }
    }

    #[tokio::test]
    async fn unreadable_override_falls_back_to_default() {
        let mut cfg = Config::default();
        cfg.prompts.mcp_instructions = Some("/definitely/not/a/real/prompt.md".to_string());
        assert_eq!(mcp_instructions(&cfg).await, MCP_INSTRUCTIONS_DEFAULT);
    }

    // --- summary override injection ---

    #[tokio::test]
    async fn summary_override_unset_leaves_policy_untouched() {
        let cfg = Config::default();
        let out = apply_summary_override(Some(policy()), &cfg).await.unwrap();
        assert_eq!(out.summary_prompt, None);
    }

    #[tokio::test]
    async fn summary_override_fills_empty_policy_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sum.md");
        std::fs::write(&path, "CUSTOM SUMMARY").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.summary = Some(path.to_string_lossy().to_string());
        let out = apply_summary_override(Some(policy()), &cfg).await.unwrap();
        assert_eq!(out.summary_prompt.as_deref(), Some("CUSTOM SUMMARY"));
    }

    #[tokio::test]
    async fn summary_override_does_not_clobber_env_value() {
        // An agent-env DAIMONOS_AGENT_SUMMARY_PROMPT has already populated
        // policy.summary_prompt; the config path must not override it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sum.md");
        std::fs::write(&path, "CONFIG SUMMARY").unwrap();
        let mut cfg = Config::default();
        cfg.prompts.summary = Some(path.to_string_lossy().to_string());
        let mut p = policy();
        p.summary_prompt = Some("ENV SUMMARY".to_string());
        let out = apply_summary_override(Some(p), &cfg).await.unwrap();
        assert_eq!(out.summary_prompt.as_deref(), Some("ENV SUMMARY"));
    }

    #[tokio::test]
    async fn summary_override_on_disabled_compaction_is_none() {
        let mut cfg = Config::default();
        cfg.prompts.summary = Some("/whatever.md".to_string());
        assert!(apply_summary_override(None, &cfg).await.is_none());
    }

    // --- baseline dump / scaffold (vikunja #980) ---

    #[test]
    fn default_by_name_covers_every_prompt_name() {
        for name in PROMPT_NAMES {
            assert!(
                default_by_name(name).is_some(),
                "missing default for {name}"
            );
        }
        assert!(default_by_name("nope").is_none());
    }

    #[test]
    fn dump_defaults_writes_all_prompts_with_matching_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("prompts");
        let report = dump_defaults(Some(&target.to_string_lossy()), false).unwrap();
        assert_eq!(report.written.len(), PROMPT_NAMES.len());
        assert!(report.skipped.is_empty());
        for name in PROMPT_NAMES {
            let content = std::fs::read_to_string(target.join(prompt_filename(name))).unwrap();
            assert_eq!(content, default_by_name(name).unwrap());
        }
    }

    #[test]
    fn dump_defaults_skips_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("prompts");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("summary.md"), "USER EDITED").unwrap();

        let report = dump_defaults(Some(&target.to_string_lossy()), false).unwrap();
        assert!(report.skipped.contains(&"summary".to_string()));
        assert_eq!(report.written.len(), PROMPT_NAMES.len() - 1);
        // The user's edited file is untouched.
        assert_eq!(
            std::fs::read_to_string(target.join("summary.md")).unwrap(),
            "USER EDITED"
        );
    }

    #[test]
    fn dump_defaults_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("prompts");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("summary.md"), "USER EDITED").unwrap();

        let report = dump_defaults(Some(&target.to_string_lossy()), true).unwrap();
        assert!(report.skipped.is_empty());
        assert_eq!(
            std::fs::read_to_string(target.join("summary.md")).unwrap(),
            SUMMARY_DEFAULT
        );
    }

    #[test]
    fn prompts_toml_block_lists_all_keys_under_dir() {
        let block = prompts_toml_block(std::path::Path::new("/tmp/p"));
        assert!(block.starts_with("[prompts]\n"));
        for name in PROMPT_NAMES {
            assert!(block.contains(&format!("{name} =")) || block.contains(&format!("{name}  ")));
            assert!(block.contains(&format!("/tmp/p/{}", prompt_filename(name))));
        }
    }
}
