use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::{AcpArgs, AgentArgs, ChatArgs};
use crate::{
    acp_cmd, agent, agent_cmd, agent_env, analytics, chat_cmd, config, paths, prompts, providers,
    safety, session_store,
};

fn try_build_provider(
    provider: &str,
    api_key: &str,
    base_url: &str,
) -> Result<Box<dyn providers::LlmProvider>, String> {
    match provider {
        "openrouter" => providers::openrouter::OpenRouterProvider::new(
            api_key.to_string(),
            base_url.to_string(),
        )
        .map(|provider| Box::new(provider) as Box<dyn providers::LlmProvider>),
        "anthropic" => Ok(Box::new(
            providers::anthropic::AnthropicProvider::new(api_key.to_string())
                .with_base_url(base_url.to_string()),
        )),
        "openai" => {
            providers::openai::OpenAiProvider::new(api_key.to_string(), base_url.to_string())
                .map(|provider| Box::new(provider) as Box<dyn providers::LlmProvider>)
        }
        other => Err(format!(
            "unsupported provider: {other} (valid: openrouter, anthropic, openai)"
        )),
    }
}

fn build_provider(
    effective_provider: &str,
    agent: &agent_env::AgentEnv,
) -> anyhow::Result<Box<dyn providers::LlmProvider>> {
    try_build_provider(effective_provider, &agent.api_key, &agent.base_url)
        .map_err(anyhow::Error::msg)
}

pub async fn run_agent(
    args: AgentArgs,
    workspace: &Path,
    cfg: Arc<config::Config>,
    token_log: Option<PathBuf>,
) -> anyhow::Result<()> {
    let AgentArgs {
        task,
        model,
        provider,
        dry_run,
        agent_env,
    } = args;

    struct DryRunProvider;
    #[async_trait]
    impl providers::LlmProvider for DryRunProvider {
        async fn complete(
            &self,
            _: &providers::Context,
            _: &providers::CompleteOpts,
        ) -> providers::LlmResponse {
            unreachable!("complete() called in dry-run mode")
        }
    }

    if dry_run {
        let args = agent_cmd::AgentCmdArgs {
            task,
            model,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log,
        };
        let result = agent_cmd::run_agent(
            &DryRunProvider,
            workspace,
            cfg,
            args,
            &mut std::io::stdout(),
        )
        .await?;
        check_agent_result(&result)?;
        return Ok(());
    }

    let agent = load_agent_env(agent_env)?;
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let llm = build_provider(&effective_provider, &agent)?;
    let analytics_store = if cfg.analytics.enabled {
        let db_path = cfg.analytics.resolved_db_path();
        analytics::AnalyticsStore::new(&db_path, cfg.analytics.retention_days)
            .ok()
            .map(Arc::new)
    } else {
        None
    };
    let approve_fn = if agent.approval_mode == "auto" {
        None
    } else {
        Some(safety::SafetyPolicy::stdin_approve_fn())
    };
    let args = agent_cmd::AgentCmdArgs {
        task,
        model: Some(effective_model),
        dry_run: false,
        safety: Some(agent.to_safety_policy(approve_fn)),
        analytics: analytics_store,
        token_log,
    };
    let result =
        agent_cmd::run_agent(llm.as_ref(), workspace, cfg, args, &mut std::io::stdout()).await?;
    check_agent_result(&result)
}

pub async fn run_chat(
    args: ChatArgs,
    workspace: &Path,
    cfg: Arc<config::Config>,
    token_log: Option<PathBuf>,
) -> anyhow::Result<()> {
    let ChatArgs {
        model,
        provider,
        agent_env,
        resume,
        list,
    } = args;
    let sessions_dir = paths::home_dir().map(|home| home.join(".daimonos").join("chat-sessions"));
    if list {
        let sessions = sessions_dir
            .map(|directory| session_store::SessionStore::new(directory).list())
            .unwrap_or_default();
        println!("{}", chat_cmd::format_session_list(&sessions));
        return Ok(());
    }

    let agent = load_agent_env(agent_env)?;
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let model_explicit = model.is_some();
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let llm = build_provider(&effective_provider, &agent)?;
    let compaction = agent
        .resolve_compaction(llm.as_ref(), &effective_model)
        .await
        .map_err(|error| anyhow::anyhow!("agent config: {error}"))?;
    let compaction = prompts::apply_summary_override(compaction, &cfg).await;
    let approve_fn = if agent.approval_mode == "auto" {
        None
    } else {
        Some(safety::SafetyPolicy::stdin_approve_fn())
    };
    let safety = agent.to_safety_policy(approve_fn);
    chat_cmd::run_chat(
        llm,
        workspace,
        cfg,
        effective_model,
        model_explicit,
        Some(safety),
        token_log,
        sessions_dir,
        resume,
        compaction,
    )
    .await
}

pub async fn run_acp(
    args: AcpArgs,
    workspace: &Path,
    cfg: Arc<config::Config>,
    token_log: Option<PathBuf>,
) -> anyhow::Result<()> {
    let AcpArgs {
        model,
        provider,
        agent_env,
    } = args;
    let agent = load_agent_env(agent_env)?;
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let mut models = agent.models.clone();
    if !models.iter().any(|model| model == &effective_model) {
        models.insert(0, effective_model.clone());
    }
    let make_provider: acp_cmd::ProviderFactory = {
        let provider = effective_provider.clone();
        let api_key = agent.api_key.clone();
        let base_url = agent.base_url.clone();
        Arc::new(move || try_build_provider(&provider, &api_key, &base_url))
    };
    let compaction_follows_model = matches!(
        &agent.compaction,
        agent_env::CompactionConfig::NeedsWindow(_)
    );
    let probe = make_provider().map_err(|error| anyhow::anyhow!("agent config: {error}"))?;
    let compaction = agent
        .resolve_compaction(probe.as_ref(), &effective_model)
        .await
        .map_err(|error| anyhow::anyhow!("agent config: {error}"))?;
    let compaction = prompts::apply_summary_override(compaction, &cfg).await;
    let safety = agent.to_safety_policy(None);
    let sessions_dir = paths::home_dir().map(|home| home.join(".daimonos").join("acp-sessions"));
    let analytics_store = if cfg.analytics.enabled {
        let db_path = cfg.analytics.resolved_db_path();
        match analytics::AnalyticsStore::new(&db_path, cfg.analytics.retention_days) {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                eprintln!("analytics: disabled (init failed: {error})");
                None
            }
        }
    } else {
        None
    };
    acp_cmd::run_acp(
        make_provider,
        workspace,
        cfg,
        effective_model,
        models,
        safety,
        token_log,
        sessions_dir,
        crate::session_core::SessionCompaction::new(compaction, compaction_follows_model),
        analytics_store,
        agent.timestamp_turns,
    )
    .await
}

fn load_agent_env(path: Option<PathBuf>) -> anyhow::Result<agent_env::AgentEnv> {
    agent_env::AgentEnv::load(path).map_err(|error| anyhow::anyhow!("agent config: {error}"))
}

fn check_agent_result(result: &agent::AgentResult) -> anyhow::Result<()> {
    if result.stop_reason == providers::StopReason::Error {
        let message = result.error_message.as_deref().unwrap_or("unknown error");
        anyhow::bail!("agent error: {message}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_openai_provider_builds_and_reports_known_window() {
        let provider = try_build_provider("openai", "key", "https://api.openai.com/v1")
            .expect("native OpenAI provider");
        assert_eq!(
            provider.context_window("gpt-5.6-sol").await,
            Some(1_050_000)
        );
        assert!(!provider.supports_images());
    }

    #[test]
    fn unsupported_provider_names_openai_in_valid_set() {
        let error = try_build_provider("ollama", "key", "http://localhost")
            .err()
            .expect("unsupported provider error");
        assert!(error.contains("openai"));
    }

    #[test]
    fn agent_provider_error_returns_to_caller_for_ordered_shutdown() {
        let result = agent::AgentResult {
            messages: Vec::new(),
            usage: Default::default(),
            stop_reason: providers::StopReason::Error,
            error_message: Some("provider unavailable".to_string()),
            last_call_usage: Default::default(),
            context_overflow: false,
        };

        let error = check_agent_result(&result).unwrap_err();

        assert!(error.to_string().contains("provider unavailable"));
    }
}
