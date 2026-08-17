use async_trait::async_trait;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::{AcpArgs, AgentArgs, ChatArgs, SessionDaemonArgs};
use crate::{
    acp_cmd, agent, agent_cmd, agent_env, analytics, chat_cmd, config, paths, prompts, providers,
    safety, session_daemon, session_factory, session_store, tui,
};

fn try_build_provider(
    provider: &str,
    api_key: &str,
    base_url: &str,
    prompt_cache: bool,
    // #1107: bounded HTTP/SSE deadlines. Passed in rather than defaulted inside
    // each adapter so `[provider_timeouts]` in config.toml actually applies —
    // config that silently does nothing is worse than no config.
    timeouts: providers::ProviderTimeouts,
) -> Result<Box<dyn providers::LlmProvider>, String> {
    match provider {
        "openrouter" => providers::openrouter::OpenRouterProvider::new(
            api_key.to_string(),
            base_url.to_string(),
        )
        .map(|p| Box::new(p.with_timeouts(timeouts)) as Box<dyn providers::LlmProvider>),
        "anthropic" => Ok(Box::new(
            providers::anthropic::AnthropicProvider::new(api_key.to_string())
                .with_base_url(base_url.to_string())
                .with_prompt_cache(prompt_cache)
                .with_timeouts(timeouts),
        )),
        "openai" => {
            providers::openai::OpenAiProvider::new(api_key.to_string(), base_url.to_string())
                .map(|p| Box::new(p.with_timeouts(timeouts)) as Box<dyn providers::LlmProvider>)
        }
        other => Err(format!(
            "unsupported provider: {other} (valid: openrouter, anthropic, openai)"
        )),
    }
}

fn build_provider(
    effective_provider: &str,
    agent: &agent_env::AgentEnv,
    timeouts: providers::ProviderTimeouts,
) -> anyhow::Result<Box<dyn providers::LlmProvider>> {
    try_build_provider(
        effective_provider,
        &agent.api_key,
        &agent.base_url,
        agent.prompt_cache,
        timeouts,
    )
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
        interactive,
        no_color,
        print,
        model,
        provider,
        dry_run,
        agent_env,
    } = args;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // Pre-resolution intent capture: the operator asked for interactive but
    // no TTY is attached. Only the Print fallback's require_agent_task error
    // text consumes this, to explain why a task argument became mandatory.
    let interactive_fell_back = interactive && !print && !dry_run && !tty;
    let mode = tui::resolve_agent_mode(interactive, print, dry_run, tty);

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

    if mode == tui::AgentMode::DryRun {
        let task = require_agent_task(task, mode, false)?;
        let args = agent_cmd::AgentCmdArgs {
            task,
            model,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log,
            // Dry-run never calls the provider and does not load the agent env,
            // so effort is immaterial here; use the default.
            thinking: providers::ThinkingLevel::default(),
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

    let task = if mode == tui::AgentMode::Print {
        Some(require_agent_task(task, mode, interactive_fell_back)?)
    } else {
        task
    };
    let agent = load_agent_env(agent_env)?;
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let llm = build_provider(
        &effective_provider,
        &agent,
        providers::ProviderTimeouts::from_config(&cfg.provider_timeouts),
    )?;
    let analytics_store = if cfg.analytics.enabled {
        let db_path = cfg.analytics.resolved_db_path();
        analytics::AnalyticsStore::new(&db_path, cfg.analytics.retention_days)
            .ok()
            .map(Arc::new)
    } else {
        None
    };

    if mode == tui::AgentMode::Interactive {
        let compaction = agent
            .resolve_compaction(llm.as_ref(), &effective_model)
            .await
            .map_err(|error| anyhow::anyhow!("agent config: {error}"))?;
        let compaction = prompts::apply_summary_override(compaction, &cfg).await;
        let mut models = agent.models.clone();
        if !models.iter().any(|model| model == &effective_model) {
            models.insert(0, effective_model.clone());
        }
        return tui::run_tui(
            llm,
            workspace,
            cfg,
            tui::TuiOptions {
                initial_prompt: task,
                no_color,
                model: effective_model,
                models,
                safety: agent.to_safety_policy(None),
                token_log,
                compaction,
                analytics: analytics_store,
                thinking: agent.thinking,
            },
        )
        .await;
    }

    let task = require_agent_task(task, mode, false)?;
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
        thinking: agent.thinking.clone(),
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
    let llm = build_provider(
        &effective_provider,
        &agent,
        providers::ProviderTimeouts::from_config(&cfg.provider_timeouts),
    )?;
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
        let prompt_cache = agent.prompt_cache;
        let timeouts = providers::ProviderTimeouts::from_config(&cfg.provider_timeouts);
        Arc::new(move || try_build_provider(&provider, &api_key, &base_url, prompt_cache, timeouts))
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
    let sessions_dir = paths::agent_sessions_dir();
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
        agent.thinking.clone(),
    )
    .await
}

pub async fn run_session_daemon(
    args: SessionDaemonArgs,
    workspace: &Path,
    cfg: Arc<config::Config>,
    token_log: Option<PathBuf>,
) -> anyhow::Result<()> {
    let SessionDaemonArgs {
        socket,
        model,
        provider,
        agent_env,
        remote_listen,
        remote_origins,
        remote_allow_always,
        remote_trust_proxy_headers,
    } = args;
    let agent = load_agent_env(agent_env)?;
    let effective_provider = provider.unwrap_or_else(|| agent.provider.clone());
    let effective_model = model.unwrap_or_else(|| agent.model.clone());
    let make_provider: session_factory::ProviderFactory = {
        let provider = effective_provider.clone();
        let api_key = agent.api_key.clone();
        let base_url = agent.base_url.clone();
        let prompt_cache = agent.prompt_cache;
        let timeouts = providers::ProviderTimeouts::from_config(&cfg.provider_timeouts);
        Arc::new(move || try_build_provider(&provider, &api_key, &base_url, prompt_cache, timeouts))
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
    let analytics_store = if cfg.analytics.enabled {
        analytics::AnalyticsStore::new(
            &cfg.analytics.resolved_db_path(),
            cfg.analytics.retention_days,
        )
        .ok()
        .map(Arc::new)
    } else {
        None
    };
    let sessions_dir = paths::daemon_sessions_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for session persistence"))?;
    let services = Arc::new(
        crate::provisioning::build_tool_services(
            workspace,
            &cfg,
            true,
            true,
            analytics_store.clone(),
        )
        .await,
    );
    let mut models = agent.models.clone();
    if !models.iter().any(|candidate| candidate == &effective_model) {
        models.insert(0, effective_model.clone());
    }
    let factory = Arc::new(session_factory::AgentSessionFactory::new(
        make_provider,
        workspace.to_path_buf(),
        Arc::clone(&cfg),
        effective_model,
        models,
        agent.thinking.clone(),
        agent.to_safety_policy(None),
        token_log,
        session_store::SessionStore::new(sessions_dir),
        crate::session_core::SessionCompaction::new(compaction, compaction_follows_model),
        services,
    ));
    let daemon = Arc::new(session_daemon::SessionDaemon::with_factory(
        cfg.session.max_sessions,
        cfg.session.max_clients_per_session,
        cfg.session.event_queue_capacity,
        cfg.session.snapshot_entries,
        (cfg.session.idle_retention_secs > 0)
            .then(|| std::time::Duration::from_secs(cfg.session.idle_retention_secs)),
        cfg.session.session_list_page_size,
        std::time::Duration::from_secs(cfg.session.shutdown_grace_secs),
        factory,
    ));
    let socket_path = socket.unwrap_or_else(|| cfg.session.resolved_socket_path(workspace));
    eprintln!(
        "daimonos session daemon listening on {}",
        socket_path.display()
    );
    let serve = Arc::clone(&daemon).serve_unix(
        socket_path,
        cfg.session.max_frame_bytes,
        cfg.session.protocol_limits(),
        std::time::Duration::from_millis(cfg.session.accept_error_backoff_ms),
    );
    tokio::pin!(serve);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = if let Some(remote_listen) = remote_listen {
        if !remote_listen.ip().is_loopback() {
            anyhow::bail!(
                "--remote-listen must use a loopback address; publish it through a TLS reverse proxy"
            );
        }
        let listener = tokio::net::TcpListener::bind(remote_listen).await?;
        let authenticator = Arc::new(crate::remote_auth::PairingAuthority::new(
            std::time::Duration::from_secs(cfg.session.remote_pairing_ttl_secs),
            cfg.session.remote_max_paired_devices,
        ));
        let claim = authenticator.create_claim();
        eprintln!(
            "daimonos remote gateway listening on loopback {remote_listen}, WebSocket path /v2/ws"
        );
        print_remote_pairing_claim(&claim);
        let consent_task = tokio::spawn(remote_pairing_consent_loop(
            Arc::clone(&authenticator),
            remote_allow_always,
            std::time::Duration::from_secs(cfg.session.remote_pairing_ttl_secs),
        ));
        let gateway = crate::remote_gateway::RemoteGateway::new(
            Arc::clone(&daemon),
            authenticator,
            cfg.session.protocol_limits(),
            crate::remote_gateway::RemoteGatewayConfig {
                allowed_origins: remote_origins.into_iter().collect(),
                max_frame_bytes: cfg.session.max_frame_bytes,
                pairing_wait: std::time::Duration::from_secs(cfg.session.remote_pairing_wait_secs),
                auth_timeout: std::time::Duration::from_secs(cfg.session.remote_auth_timeout_secs),
                heartbeat_interval: std::time::Duration::from_secs(
                    cfg.session.remote_heartbeat_interval_secs,
                ),
                heartbeat_timeout: std::time::Duration::from_secs(
                    cfg.session.remote_heartbeat_timeout_secs,
                ),
                max_messages_per_second: cfg.session.remote_max_messages_per_second,
                max_connections: cfg.session.remote_max_connections,
                admission_attempts_per_minute: cfg.session.remote_admission_attempts_per_minute,
                max_unauthenticated_per_ip: cfg.session.remote_max_unauthenticated_per_ip,
                trust_proxy_headers: remote_trust_proxy_headers,
                max_admission_peers: cfg.session.remote_max_admission_peers,
            },
        );
        let gateway_serve = gateway.serve(listener);
        tokio::pin!(gateway_serve);
        let result = tokio::select! {
            result = &mut serve => result.map_err(anyhow::Error::from),
            result = &mut gateway_serve => result.map_err(anyhow::Error::from),
            signal = tokio::signal::ctrl_c() => {
                signal?;
                Ok(())
            }
            _ = terminate.recv() => Ok(()),
        };
        consent_task.abort();
        result
    } else {
        tokio::select! {
            result = &mut serve => result.map_err(anyhow::Error::from),
            signal = tokio::signal::ctrl_c() => {
                signal?;
                Ok(())
            }
            _ = terminate.recv() => Ok(()),
        }
    };
    daemon.shutdown().await;
    result
}

async fn remote_pairing_consent_loop(
    authenticator: Arc<crate::remote_auth::PairingAuthority>,
    allow_always: bool,
    claim_ttl: std::time::Duration,
) {
    use tokio::io::AsyncBufReadExt;

    let mut announced = std::collections::HashSet::new();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
    let renewal_interval = claim_ttl
        .checked_div(2)
        .unwrap_or(std::time::Duration::from_secs(1))
        .max(std::time::Duration::from_secs(1));
    let mut next_claim_at = tokio::time::Instant::now() + renewal_interval;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let mut submitted = false;
                let pending_pairings = authenticator.pending_pairings();
                let pending_ids: std::collections::HashSet<_> = pending_pairings
                    .iter()
                    .map(|pending| pending.id.clone())
                    .collect();
                announced.retain(|pairing_id| pending_ids.contains(pairing_id));
                for pending in pending_pairings {
                    if announced.insert(pending.id.clone()) {
                        submitted = true;
                        let capability_names = pending
                            .requested_capabilities
                            .iter()
                            .map(remote_capability_name)
                            .collect::<Vec<_>>()
                            .join(" ");
                        eprintln!(
                            "remote pairing request\n  id: {}\n  label: {}\n  fingerprint: {}\n  capabilities: {:?}\n\
                             Type `approve {} {}` with the subset to grant, or `deny {}`.",
                            pending.id,
                            pending.label,
                            pending.fingerprint,
                            pending.requested_capabilities,
                            pending.id,
                            capability_names,
                            pending.id,
                        );
                    }
                }
                if submitted || tokio::time::Instant::now() >= next_claim_at {
                    print_remote_pairing_claim(&authenticator.create_claim());
                    next_claim_at = tokio::time::Instant::now() + renewal_interval;
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else {
                    return;
                };
                let words: Vec<_> = line.split_whitespace().collect();
                match words.as_slice() {
                    ["approve", pairing_id, capability_names @ ..]
                        if !capability_names.is_empty() =>
                    {
                        let pending = authenticator
                            .pending_pairings()
                            .into_iter()
                            .find(|pending| pending.id == *pairing_id);
                        let Some(pending) = pending else {
                            eprintln!("remote pairing not found: {pairing_id}");
                            continue;
                        };
                        let capabilities: Result<Vec<_>, _> = capability_names
                            .iter()
                            .map(|name| parse_remote_capability(name))
                            .collect();
                        let Ok(capabilities) = capabilities else {
                            eprintln!("remote approval contains an unknown capability");
                            continue;
                        };
                        if !allow_always
                            && capabilities.contains(
                                &crate::session_protocol::ClientCapability::ApproveAlways,
                            )
                        {
                            eprintln!("remote approve_always is disabled by host policy");
                            continue;
                        }
                        match authenticator.approve(pairing_id, capabilities) {
                            Ok(_) => eprintln!("remote device approved: {}", pending.fingerprint),
                            Err(error) => eprintln!("remote approval failed: {error:?}"),
                        }
                    }
                    ["deny", pairing_id] => {
                        match authenticator.deny(pairing_id) {
                            Ok(()) => eprintln!("remote pairing denied: {pairing_id}"),
                            Err(error) => eprintln!("remote denial failed: {error:?}"),
                        }
                    }
                    ["revoke", device_id] => {
                        if authenticator.revoke_device(device_id) {
                            eprintln!("remote device revoked: {device_id}");
                        } else {
                            eprintln!("remote device not found: {device_id}");
                        }
                    }
                    _ => eprintln!(
                        "remote command must be `approve <pairing-id> <capability>...`, \
                         `deny <pairing-id>`, or `revoke <device-id>`"
                    ),
                }
            }
        }
    }
}

fn print_remote_pairing_claim(claim: &crate::remote_auth::PairingClaim) {
    eprintln!(
        "pairing claim (single-use, {}s): {}",
        claim.expires_in_secs, claim.secret
    );
}

fn parse_remote_capability(name: &str) -> Result<crate::session_protocol::ClientCapability, ()> {
    use crate::session_protocol::ClientCapability;

    match name {
        "observe" => Ok(ClientCapability::Observe),
        "prompt" => Ok(ClientCapability::Prompt),
        "configure" => Ok(ClientCapability::Configure),
        "interrupt" => Ok(ClientCapability::Interrupt),
        "stop" => Ok(ClientCapability::Stop),
        "approve_once" => Ok(ClientCapability::ApproveOnce),
        "approve_always" => Ok(ClientCapability::ApproveAlways),
        _ => Err(()),
    }
}

fn remote_capability_name(capability: &crate::session_protocol::ClientCapability) -> &'static str {
    use crate::session_protocol::ClientCapability;

    match capability {
        ClientCapability::Observe => "observe",
        ClientCapability::Prompt => "prompt",
        ClientCapability::Configure => "configure",
        ClientCapability::Interrupt => "interrupt",
        ClientCapability::Stop => "stop",
        ClientCapability::ApproveOnce => "approve_once",
        ClientCapability::ApproveAlways => "approve_always",
    }
}

fn load_agent_env(path: Option<PathBuf>) -> anyhow::Result<agent_env::AgentEnv> {
    agent_env::AgentEnv::load(path).map_err(|error| anyhow::anyhow!("agent config: {error}"))
}

fn require_agent_task(
    task: Option<String>,
    mode: tui::AgentMode,
    interactive_fell_back: bool,
) -> anyhow::Result<String> {
    task.filter(|task| !task.trim().is_empty()).ok_or_else(|| {
        let hint = if interactive_fell_back {
            "--interactive was disabled because stdin or stdout is not a TTY"
        } else if mode == tui::AgentMode::Print {
            "pass a task or launch a TTY with --interactive"
        } else {
            "pass a task"
        };
        anyhow::anyhow!("agent task is required in {} mode; {hint}", mode.as_str())
    })
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
        let provider = try_build_provider(
            "openai",
            "key",
            "https://api.openai.com/v1",
            false,
            providers::ProviderTimeouts::default(),
        )
        .expect("native OpenAI provider");
        assert_eq!(
            provider.context_window("gpt-5.6-sol").await,
            Some(1_050_000)
        );
        assert!(!provider.supports_images());
    }

    #[test]
    fn unsupported_provider_names_openai_in_valid_set() {
        let error = try_build_provider(
            "ollama",
            "key",
            "http://localhost",
            false,
            providers::ProviderTimeouts::default(),
        )
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
