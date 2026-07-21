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
        other => Err(format!(
            "unsupported provider: {other} (valid: openrouter, anthropic)"
        )),
    }
}

fn build_provider(
    effective_provider: &str,
    agent: &agent_env::AgentEnv,
) -> Box<dyn providers::LlmProvider> {
    match try_build_provider(effective_provider, &agent.api_key, &agent.base_url) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
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
        exit_on_agent_error(&result);
        return Ok(());
    }

    let agent = load_agent_env(agent_env);
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let llm = build_provider(&effective_provider, &agent);
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
    exit_on_agent_error(&result);
    Ok(())
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
    let sessions_dir = paths::home_dir().map(|home| home.join(".daimonos/chat-sessions"));
    if list {
        let sessions = sessions_dir
            .map(|directory| session_store::SessionStore::new(directory).list())
            .unwrap_or_default();
        println!("{}", chat_cmd::format_session_list(&sessions));
        return Ok(());
    }

    let agent = load_agent_env(agent_env);
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let model_explicit = model.is_some();
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let llm = build_provider(&effective_provider, &agent);
    let compaction = match agent
        .resolve_compaction(llm.as_ref(), &effective_model)
        .await
    {
        Ok(compaction) => compaction,
        Err(error) => exit_agent_config(error),
    };
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
    let agent = load_agent_env(agent_env);
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
    let compaction = match make_provider() {
        Ok(probe) => match agent
            .resolve_compaction(probe.as_ref(), &effective_model)
            .await
        {
            Ok(compaction) => compaction,
            Err(error) => exit_agent_config(error),
        },
        Err(error) => exit_agent_config(error),
    };
    let compaction = prompts::apply_summary_override(compaction, &cfg).await;
    let safety = agent.to_safety_policy(None);
    let sessions_dir = paths::home_dir().map(|home| home.join(".daimonos/acp-sessions"));
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
        acp_cmd::AcpCompaction::new(compaction, compaction_follows_model),
        analytics_store,
    )
    .await
}

fn load_agent_env(path: Option<PathBuf>) -> agent_env::AgentEnv {
    match agent_env::AgentEnv::load(path) {
        Ok(agent) => agent,
        Err(error) => exit_agent_config(error),
    }
}

fn exit_agent_config(error: impl std::fmt::Display) -> ! {
    eprintln!("agent config: {error}");
    std::process::exit(2);
}

fn exit_on_agent_error(result: &agent::AgentResult) {
    if result.stop_reason == providers::StopReason::Error {
        let message = result.error_message.as_deref().unwrap_or("unknown error");
        eprintln!("agent error: {message}");
        std::process::exit(1);
    }
}
