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
            // Secondary providers may omit the base URL (multi-provider
            // support); unlike the other adapters OpenRouter's constructor
            // does not self-default.
            if base_url.trim().is_empty() {
                "https://openrouter.ai/api/v1".to_string()
            } else {
                base_url.to_string()
            },
        )
        .map(|p| Box::new(p.with_timeouts(timeouts)) as Box<dyn providers::LlmProvider>),
        "anthropic" => {
            let mut p = providers::anthropic::AnthropicProvider::new(api_key.to_string())
                .with_prompt_cache(prompt_cache)
                .with_timeouts(timeouts);
            // Empty = keep the adapter's own default endpoint.
            if !base_url.trim().is_empty() {
                p = p.with_base_url(base_url.to_string());
            }
            Ok(Box::new(p))
        }
        "openai" => {
            providers::openai::OpenAiProvider::new(api_key.to_string(), base_url.to_string())
                .map(|p| Box::new(p.with_timeouts(timeouts)) as Box<dyn providers::LlmProvider>)
        }
        other => Err(format!(
            "unsupported provider: {other} (valid: openrouter, anthropic, openai)"
        )),
    }
}

/// Startup model discovery (operator request 2026-08-17): ask the configured
/// provider for its live model catalog instead of trusting a static snapshot.
/// Precedence: live catalog > configured `DAIMONOS_AGENT_MODELS` (the
/// fallback when the fetch fails, times out, or is disabled via
/// `DAIMONOS_AGENT_MODELS_DISCOVER=off`). The active model is always present
/// (prepended if missing) because the #1240 failover chain anchors on it.
/// Bounded at 5s so a slow provider can never stall startup.
async fn discover_models(
    llm: Option<&dyn providers::LlmProvider>,
    configured: &[String],
    effective_model: &str,
    discover_live: bool,
) -> Vec<String> {
    let mut models = match llm {
        Some(llm) if discover_live => {
            match tokio::time::timeout(std::time::Duration::from_secs(5), llm.list_models()).await {
                Ok(Some(live)) => {
                    tracing::info!(
                        target: "daimonos::agent_runtime",
                        event = "models_discovered",
                        count = live.len(),
                        "populated model list from the provider's live catalog"
                    );
                    live
                }
                _ => {
                    tracing::warn!(
                        target: "daimonos::agent_runtime",
                        event = "models_discovery_unavailable",
                        "provider model catalog unavailable; using configured DAIMONOS_AGENT_MODELS"
                    );
                    configured.to_vec()
                }
            }
        }
        _ => configured.to_vec(),
    };
    if !models.iter().any(|model| model == effective_model) {
        models.insert(0, effective_model.to_string());
    }
    models
}

/// `DAIMONOS_AGENT_MODELS_DISCOVER=off|false|no|0` skips the startup catalog
/// fetch (air-gapped setups, deterministic tests).
fn models_discovery_disabled() -> bool {
    std::env::var("DAIMONOS_AGENT_MODELS_DISCOVER")
        .map(|v| {
            let v = v.trim();
            v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v == "0"
        })
        .unwrap_or(false)
}

/// Build the session's provider handle: the primary adapter alone, or a
/// [`providers::router::MultiProvider`] when `agent.env` configures
/// secondary providers (`DAIMONOS_AGENT_<NAME>_API_KEY`). The router
/// dispatches per call by model, so every downstream consumer keeps seeing
/// one `dyn LlmProvider`.
fn try_build_session_provider(
    effective_provider: &str,
    api_key: &str,
    base_url: &str,
    prompt_cache: bool,
    secondaries: &[agent_env::SecondaryProvider],
    timeouts: providers::ProviderTimeouts,
) -> Result<Box<dyn providers::LlmProvider>, String> {
    let primary = try_build_provider(
        effective_provider,
        api_key,
        base_url,
        prompt_cache,
        timeouts,
    )?;
    // A secondary matching the (possibly --provider-overridden) primary is
    // redundant: the primary's credentials already serve those models.
    let secondaries: Vec<&agent_env::SecondaryProvider> = secondaries
        .iter()
        .filter(|s| s.name != effective_provider)
        .collect();
    if secondaries.is_empty() {
        return Ok(primary);
    }
    let mut adapters = vec![(effective_provider.to_string(), primary)];
    for s in secondaries {
        let adapter = try_build_provider(&s.name, &s.api_key, &s.base_url, prompt_cache, timeouts)
            .map_err(|e| format!("secondary provider {}: {e}", s.name))?;
        adapters.push((s.name.clone(), adapter));
    }
    tracing::info!(
        target: "daimonos::agent_runtime",
        event = "multi_provider",
        adapters = adapters.len(),
        "routing across multiple providers"
    );
    Ok(Box::new(providers::router::MultiProvider::new(adapters)))
}

fn build_provider(
    effective_provider: &str,
    agent: &agent_env::AgentEnv,
    timeouts: providers::ProviderTimeouts,
) -> anyhow::Result<Box<dyn providers::LlmProvider>> {
    try_build_session_provider(
        effective_provider,
        &agent.api_key,
        &agent.base_url,
        agent.prompt_cache,
        &agent.secondary_providers,
        timeouts,
    )
    .map_err(anyhow::Error::msg)
}

fn resolve_thought_log_path(
    enabled: bool,
    override_path: Option<PathBuf>,
    home: Option<PathBuf>,
) -> anyhow::Result<Option<PathBuf>> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(match override_path {
        Some(path) => path,
        None => home
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for thought log"))?
            .join(".config")
            .join("daimonos")
            .join("thought-debug.log"),
    }))
}

#[allow(clippy::too_many_arguments)]
fn spawn_tui_controller(
    transport: crate::client_transport::UnixFrontendTransport,
    client_label: String,
    scrollback_entries: usize,
    event_queue_capacity: usize,
    reconnect_socket: PathBuf,
    max_frame_bytes: usize,
    reconnect_policy: crate::session_controller::ReconnectPolicy,
) -> crate::session_controller::SessionControllerHandle {
    let reconnect_factory: crate::session_controller::ReconnectFactory<
        crate::client_transport::UnixFrontendTransport,
    > = Arc::new(move || {
        let socket_path = reconnect_socket.clone();
        Box::pin(async move {
            let stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .map_err(crate::client_transport::TransportError::Io)?;
            crate::client_transport::UnixFrontendTransport::new(
                stream,
                format!("session daemon {}", socket_path.display()),
                max_frame_bytes,
            )
            .map_err(crate::session_client::SessionClientError::from)
        })
    });
    crate::session_controller::SessionControllerHandle::spawn_with_reconnect(
        transport,
        crate::session_protocol::ClientInfo {
            id: "tui".to_string(),
            kind: crate::session_protocol::ClientKind::Terminal,
            label: client_label,
        },
        vec![
            crate::session_protocol::ClientCapability::Observe,
            crate::session_protocol::ClientCapability::Prompt,
            crate::session_protocol::ClientCapability::Configure,
            crate::session_protocol::ClientCapability::Interrupt,
            crate::session_protocol::ClientCapability::Stop,
            crate::session_protocol::ClientCapability::ApproveOnce,
            crate::session_protocol::ClientCapability::ApproveAlways,
        ],
        scrollback_entries,
        event_queue_capacity,
        event_queue_capacity,
        reconnect_policy,
        reconnect_factory,
    )
}

pub async fn run_agent(
    args: AgentArgs,
    workspace: &Path,
    cfg: Arc<config::Config>,
    token_log: Option<PathBuf>,
    explicit_config_path: Option<PathBuf>,
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
        debug_thoughts,
        debug_thoughts_path,
    } = args;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // Pre-resolution intent capture: the operator asked for interactive but
    // no TTY is attached. Only the Print fallback's require_agent_task error
    // text consumes this, to explain why a task argument became mandatory.
    let interactive_fell_back = interactive && !print && !dry_run && !tty;
    let mode = tui::resolve_agent_mode(interactive, print, dry_run, tty);
    let thought_log = resolve_thought_log_path(
        debug_thoughts,
        debug_thoughts_path,
        crate::paths::home_dir(),
    )?;

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
            thought_log: None,
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

    if mode == tui::AgentMode::Interactive {
        let socket_path = cfg.session.resolved_socket_path(workspace);
        let bootstrap = crate::session_bootstrap::connect_or_spawn(
            &crate::session_bootstrap::BootstrapOptions {
                workspace,
                config_path: explicit_config_path.as_deref(),
                socket_path: &socket_path,
                provider: provider.as_deref(),
                model: model.as_deref(),
                agent_env: agent_env.as_deref(),
                max_frame_bytes: cfg.session.max_frame_bytes,
                timeout: std::time::Duration::from_secs(cfg.session.bootstrap_timeout_secs),
                retry_interval: std::time::Duration::from_millis(
                    cfg.session.bootstrap_retry_interval_ms,
                ),
            },
        )
        .await?;
        match bootstrap.launch_identity {
            crate::session_bootstrap::LaunchIdentity::NotRequested
            | crate::session_bootstrap::LaunchIdentity::Matched => {}
            crate::session_bootstrap::LaunchIdentity::Mismatched => {
                anyhow::bail!(
                    "explicit config/provider/agent-env selection differs from the running \
                     session daemon; restart the daemon with the requested selection"
                );
            }
            crate::session_bootstrap::LaunchIdentity::Unavailable => {
                anyhow::bail!(
                    "the running session daemon does not publish a comparable launch identity; \
                     restart it with this daimonos version before applying explicit \
                     config/provider/agent-env selection"
                );
            }
        }
        let reconnect_policy = crate::session_controller::ReconnectPolicy {
            attempts: cfg.session.reconnect_attempts,
            initial_backoff: std::time::Duration::from_millis(
                cfg.session.reconnect_initial_backoff_ms,
            ),
            max_backoff: std::time::Duration::from_millis(cfg.session.reconnect_max_backoff_ms),
        };
        let client_label = format!("terminal {}", workspace.display());
        let controller = spawn_tui_controller(
            bootstrap.transport,
            client_label.clone(),
            cfg.tui.scrollback_entries,
            cfg.session.event_queue_capacity,
            socket_path.clone(),
            cfg.session.max_frame_bytes,
            reconnect_policy,
        );
        let switch_socket = socket_path.clone();
        let switch_scrollback = cfg.tui.scrollback_entries;
        let switch_event_capacity = cfg.session.event_queue_capacity;
        let switch_max_frame = cfg.session.max_frame_bytes;
        let switch_label = client_label;
        let controller_factory: crate::tui::ControllerFactory = Arc::new(move || {
            let socket_path = switch_socket.clone();
            let client_label = switch_label.clone();
            Box::pin(async move {
                let stream = tokio::net::UnixStream::connect(&socket_path).await?;
                let transport = crate::client_transport::UnixFrontendTransport::new(
                    stream,
                    format!("session daemon {}", socket_path.display()),
                    switch_max_frame,
                )?;
                Ok(spawn_tui_controller(
                    transport,
                    client_label,
                    switch_scrollback,
                    switch_event_capacity,
                    socket_path,
                    switch_max_frame,
                    reconnect_policy,
                ))
            })
        });
        return tui::run_tui(
            controller,
            tui::TuiOptions {
                initial_prompt: task,
                no_color,
                model_override: model,
                history_entries: cfg.tui.history_entries,
                command_timeout: std::time::Duration::from_secs(
                    cfg.session.client_command_timeout_secs,
                ),
                controller_factory: Some(controller_factory),
                switch_policy: crate::tui::SwitchPolicy {
                    retry_attempts: cfg.session.switch_attach_retry_attempts,
                    retry_backoff: std::time::Duration::from_millis(
                        cfg.session.switch_attach_retry_backoff_ms,
                    ),
                },
            },
        )
        .await;
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
        thought_log,
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
        let busy_timeout =
            std::time::Duration::from_millis(cfg.session.session_store_busy_timeout_ms);
        let sessions = sessions_dir
            .map(|directory| {
                session_store::SessionStore::new(directory)
                    .with_busy_timeout(busy_timeout)
                    .list()
            })
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
    let make_provider: acp_cmd::ProviderFactory = {
        let provider = effective_provider.clone();
        let api_key = agent.api_key.clone();
        let base_url = agent.base_url.clone();
        let prompt_cache = agent.prompt_cache;
        let secondaries = agent.secondary_providers.clone();
        let timeouts = providers::ProviderTimeouts::from_config(&cfg.provider_timeouts);
        Arc::new(move || {
            try_build_session_provider(
                &provider,
                &api_key,
                &base_url,
                prompt_cache,
                &secondaries,
                timeouts,
            )
        })
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
    // The probe instance already exists for compaction resolution; reuse it
    // for live model discovery.
    let models = discover_models(
        Some(probe.as_ref()),
        &agent.models,
        &effective_model,
        !models_discovery_disabled(),
    )
    .await;
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
        let secondaries = agent.secondary_providers.clone();
        let timeouts = providers::ProviderTimeouts::from_config(&cfg.provider_timeouts);
        Arc::new(move || {
            try_build_session_provider(
                &provider,
                &api_key,
                &base_url,
                prompt_cache,
                &secondaries,
                timeouts,
            )
        })
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
        match analytics::AnalyticsStore::new(
            &cfg.analytics.resolved_db_path(),
            cfg.analytics.retention_days,
        ) {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                tracing::warn!(
                    target: "daimonos::analytics",
                    event = "session_daemon_analytics_init_failed",
                    error,
                );
                None
            }
        }
    } else {
        None
    };
    let sessions_dir = paths::daemon_sessions_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for session persistence"))?;
    let session_store = session_store::SessionStore::new(sessions_dir.clone()).with_busy_timeout(
        std::time::Duration::from_millis(cfg.session.session_store_busy_timeout_ms),
    );
    let workspace_identity =
        session_factory::canonical_session_workspace(workspace, cfg.session.max_label_bytes);
    let catalog = match crate::session_catalog::SessionCatalog::open(
        cfg.session.resolved_session_catalog_path(&sessions_dir),
        std::time::Duration::from_millis(cfg.session.session_catalog_busy_timeout_ms),
    ) {
        Ok(crate::session_catalog::CatalogOpen::Ready(catalog)) => Some(catalog),
        Ok(crate::session_catalog::CatalogOpen::NewerSchema { found }) => {
            tracing::warn!(
                target: "daimonos::session_catalog",
                event = "newer_catalog_schema",
                found,
                "leaving newer session catalog untouched; using fallback discovery"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                target: "daimonos::session_catalog",
                event = "catalog_open_failed",
                error = %error,
                "using fallback session discovery"
            );
            None
        }
    };
    let catalog_writer = catalog.clone().map(|catalog| {
        let _ = catalog.mark_incomplete(&workspace_identity.id);
        crate::session_catalog::SessionCatalogWriter::start(
            catalog,
            workspace_identity.id.clone(),
            cfg.session.session_catalog_pending_entries,
            cfg.session.session_catalog_write_batch,
            cfg.session.session_list_preview_bytes,
        )
    });
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
    let models = discover_models(
        Some(probe.as_ref()),
        &agent.models,
        &effective_model,
        !models_discovery_disabled(),
    )
    .await;
    let factory = session_factory::AgentSessionFactory::new(
        make_provider,
        workspace.to_path_buf(),
        Arc::clone(&cfg),
        effective_model,
        models,
        agent.thinking.clone(),
        agent.to_safety_policy(None),
        token_log,
        session_store,
        crate::session_core::SessionCompaction::new(compaction, compaction_follows_model),
        analytics_store.clone(),
        services,
    );
    let factory = Arc::new(match catalog_writer.as_ref() {
        Some(writer) => factory.with_catalog_writer(Arc::clone(writer)),
        None => factory,
    });
    let daemon = session_daemon::SessionDaemon::with_factory(
        cfg.session.max_sessions,
        cfg.session.max_clients_per_session,
        cfg.session.event_queue_capacity,
        cfg.session.timeline_entries,
        (cfg.session.idle_retention_secs > 0)
            .then(|| std::time::Duration::from_secs(cfg.session.idle_retention_secs)),
        cfg.session.session_list_page_size,
        std::time::Duration::from_secs(cfg.session.shutdown_grace_secs),
        factory,
    )
    .with_persistence_lifecycle(
        std::time::Duration::from_secs(cfg.session.persistence_eviction_extension_secs),
        std::time::Duration::from_secs(cfg.session.persistence_final_save_timeout_secs),
    )
    .with_listing_limits(
        cfg.session.session_list_preview_bytes,
        cfg.session.session_list_snapshot_entries,
        std::time::Duration::from_secs(cfg.session.session_list_snapshot_ttl_secs),
    )
    .with_global_listing_snapshot_capacity(cfg.session.session_list_snapshot_global_capacity)
    .with_discovery_fallback_limits(
        std::time::Duration::from_millis(cfg.session.session_catalog_query_timeout_ms),
        cfg.session.session_catalog_fallback_entries,
        cfg.session.session_catalog_query_concurrency,
    );
    let daemon = match (catalog, catalog_writer) {
        (Some(catalog), Some(writer)) => daemon.with_catalog_discovery(
            catalog,
            writer,
            workspace_identity.id,
            cfg.session.session_catalog_reconcile_entries,
            std::time::Duration::from_millis(cfg.session.session_catalog_reconcile_interval_ms),
            std::time::Duration::from_secs(cfg.session.session_catalog_full_rescan_secs),
            std::time::Duration::from_secs(cfg.session.session_catalog_tombstone_retention_secs),
        ),
        _ => daemon,
    };
    let daemon = Arc::new(daemon);
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
    if let Some(analytics) = analytics_store {
        let drained = analytics
            .wait_until_quiet(std::time::Duration::from_secs(
                cfg.session.shutdown_grace_secs,
            ))
            .await;
        if !drained {
            tracing::warn!(
                target: "daimonos::analytics",
                event = "session_daemon_analytics_drain_timeout",
                pending_writes = analytics.pending_writes(),
            );
        }
    }
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

    /// Stub provider whose `list_models` answer is scripted.
    struct CatalogStub(Option<Vec<String>>);

    #[async_trait::async_trait]
    impl providers::LlmProvider for CatalogStub {
        async fn complete(
            &self,
            _ctx: &providers::Context,
            _opts: &providers::CompleteOpts,
        ) -> providers::LlmResponse {
            providers::LlmResponse::error("unused")
        }

        async fn list_models(&self) -> Option<Vec<String>> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn discovery_prefers_the_live_catalog_and_anchors_the_active_model() {
        let stub = CatalogStub(Some(vec!["new-model".into(), "old-model".into()]));
        let configured = vec!["stale-a".into(), "stale-b".into()];
        let models = discover_models(Some(&stub), &configured, "active-model", true).await;
        // Live catalog replaces the static snapshot; the active model is
        // prepended because the failover chain anchors on it.
        assert_eq!(models, vec!["active-model", "new-model", "old-model"]);
    }

    #[tokio::test]
    async fn discovery_falls_back_to_configured_models_when_catalog_unavailable() {
        let stub = CatalogStub(None);
        let configured = vec!["cfg-a".into(), "cfg-b".into()];
        let models = discover_models(Some(&stub), &configured, "cfg-a", true).await;
        assert_eq!(models, vec!["cfg-a", "cfg-b"]);
        // No provider at all (factory failed): same fallback.
        let models = discover_models(None, &configured, "other", true).await;
        assert_eq!(models, vec!["other", "cfg-a", "cfg-b"]);
    }

    #[tokio::test]
    async fn discovery_policy_can_disable_the_live_catalog() {
        let stub = CatalogStub(Some(vec!["live-model".into()]));
        let configured = vec!["configured-model".into()];
        let models = discover_models(Some(&stub), &configured, "active-model", false).await;
        assert_eq!(models, vec!["active-model", "configured-model"]);
    }

    #[test]
    fn session_provider_wraps_in_a_router_only_when_secondaries_exist() {
        let timeouts = providers::ProviderTimeouts::default();
        // No secondaries: plain single-adapter handle, pre-existing behaviour.
        let single = try_build_session_provider("anthropic", "k", "", false, &[], timeouts);
        assert!(single.is_ok());
        // Secondaries: builds every adapter (empty base_url = adapter default,
        // including OpenRouter which does not self-default).
        let secondaries = vec![
            crate::agent_env::SecondaryProvider {
                name: "openai".into(),
                api_key: "k2".into(),
                base_url: String::new(),
            },
            crate::agent_env::SecondaryProvider {
                name: "openrouter".into(),
                api_key: "k3".into(),
                base_url: String::new(),
            },
        ];
        let multi = try_build_session_provider("anthropic", "k", "", false, &secondaries, timeouts);
        assert!(multi.is_ok(), "{:?}", multi.as_ref().err());
        // A secondary duplicating the primary is filtered, not doubled.
        let dup = vec![crate::agent_env::SecondaryProvider {
            name: "anthropic".into(),
            api_key: "other".into(),
            base_url: String::new(),
        }];
        assert!(try_build_session_provider("anthropic", "k", "", false, &dup, timeouts).is_ok());
    }

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
    fn thought_log_path_uses_private_default_or_exact_override() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            resolve_thought_log_path(true, None, Some(home.clone())).unwrap(),
            Some(home.join(".config/daimonos/thought-debug.log"))
        );
        assert_eq!(
            resolve_thought_log_path(true, Some(PathBuf::from("/tmp/custom-thoughts")), None)
                .unwrap(),
            Some(PathBuf::from("/tmp/custom-thoughts"))
        );
        assert_eq!(resolve_thought_log_path(false, None, None).unwrap(), None);
        assert!(resolve_thought_log_path(true, None, None).is_err());
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
