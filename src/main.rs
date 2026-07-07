mod analytics;
mod config;
mod index;
mod kgl;
mod mcp;
mod ops;
mod pipeline_cache;
mod plugins;
mod protocol;
mod script;
mod session;
mod snapshot;
mod agent;
mod agent_cmd;
mod safety;
mod providers;
mod tool_runner;
mod tool_facade;
mod tools;
mod verbosity;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// On Linux, ask the kernel to send SIGTERM to this process when its
/// parent dies. Defends against the case where the editor that spawned
/// `daimonos --mcp` crashes (or is SIGKILL'd) without orderly shutdown:
/// the orphaned daimonos would otherwise survive forever, holding
/// inotify watches and pipe fds that no one will ever read from again.
///
/// On non-Linux this is a no-op; the idle-timeout watchdog in
/// `mcp::run_mcp_server` provides equivalent protection across platforms.
#[cfg(target_os = "linux")]
fn install_parent_death_signal() {
    // PR_SET_PDEATHSIG = 1, SIGTERM = 15. Using a raw extern keeps this
    // dependency-free; libc is already in our transitive tree but we
    // don't want to take a direct dep just for one syscall.
    unsafe extern "C" {
        fn prctl(
            option: std::os::raw::c_int,
            arg2: std::os::raw::c_ulong,
            arg3: std::os::raw::c_ulong,
            arg4: std::os::raw::c_ulong,
            arg5: std::os::raw::c_ulong,
        ) -> std::os::raw::c_int;
    }
    const PR_SET_PDEATHSIG: std::os::raw::c_int = 1;
    const SIGTERM: std::os::raw::c_ulong = 15;
    let rc = unsafe { prctl(PR_SET_PDEATHSIG, SIGTERM, 0, 0, 0) };
    if rc != 0 {
        eprintln!(
            "daimonos: prctl(PR_SET_PDEATHSIG) failed (rc={rc}); parent-death cleanup unavailable"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn install_parent_death_signal() {}

/// True when `DAIMONOS_LOG_STARTUP` requests MCP stderr diagnostics (mirrors
/// `[mcp] startup_logs` / `--verbose`, but readable before config load).
fn env_requests_mcp_startup_logs() -> bool {
    match std::env::var("DAIMONOS_LOG_STARTUP") {
        Ok(s) => {
            let t = s.trim().to_ascii_lowercase();
            !t.is_empty() && t != "0" && t != "false" && t != "no"
        }
        Err(_) => false,
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Run the agent on a one-shot task and exit
    Agent {
        /// Task description for the agent
        task: String,
        /// Model override (default: claude-opus-4-8)
        #[arg(long)]
        model: Option<String>,
        /// LLM provider override (default: from [agent] config)
        #[arg(long)]
        provider: Option<String>,
        /// Print available tools and task without calling the API
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Parser)]
#[command(name = "daimonos", about = "Daimonos — agent-optimized OS layer")]
struct Cli {
    /// Unix socket path (ignored in --mcp mode)
    #[arg(short, long, default_value = "/tmp/daimonos.sock")]
    socket: PathBuf,

    /// Workspace root directory
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// Human-readable debug output (socket mode only)
    #[arg(long, default_value_t = false)]
    debug: bool,

    /// Path to config file (default: search workspace then ~/.config/daimonos/)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run as MCP server over stdio (for Cursor integration)
    #[arg(long, default_value_t = false)]
    mcp: bool,

    /// Run as MCP server over a Unix socket (per-workspace shared daemon)
    #[arg(long)]
    mcp_socket: Option<PathBuf>,

    /// Emit informational stderr during MCP startup (config source, plugins,
    /// indexer stats, idle watchdog). Cursor classifies MCP subprocess stderr
    /// as errors in the UI; omit this flag unless you are debugging daimonos.
    #[arg(long, default_value_t = false)]
    verbose: bool,

    /// Print token analytics summary and exit
    #[arg(long, default_value_t = false)]
    stats: bool,

    /// With --stats, restrict the report to a single agent-runtime
    /// session id (matches whatever `set_external_session_id` /
    /// `DAIMONOS_AGENT_SESSION_ID` set on the recording side).
    /// Useful with claude / cursor session ids: `daimonos --stats
    /// --session-id $SID`.
    #[arg(long)]
    session_id: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.mcp {
        install_parent_death_signal();
    }

    session::enhance_process_path();

    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let startup_logs_early = cli.verbose || env_requests_mcp_startup_logs();
    let quiet_cfg_stderr = cli.mcp && !startup_logs_early;
    let cfg = Arc::new(config::load(
        cli.config.as_deref(),
        &workspace,
        quiet_cfg_stderr,
    ));
    if let Err(e) = cfg.validate() {
        eprintln!(
            "config: validation error: {}",
            cfg.discord.redact_sensitive(&e)
        );
        std::process::exit(2);
    }
    // Dispatch `daimonos agent "<task>"` early — no index/watcher/plugin setup needed.
    if let Some(Commands::Agent { task, model, provider, dry_run }) = cli.command {
        // In dry-run mode we never call complete(), so skip provider init and key resolution.
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

        // Provider precedence: --provider flag overrides [agent] config (vikunja
        // #948). Mirrors --model, which overrides cfg.agent.model.
        let effective_provider = provider.unwrap_or_else(|| cfg.agent.provider.clone());
        let llm: Box<dyn providers::LlmProvider> = if dry_run {
            Box::new(DryRunProvider)
        } else {
            match effective_provider.as_str() {
                "openrouter" => {
                    match providers::openrouter::OpenRouterProvider::from_config(&cfg.agent) {
                        Ok(p) => Box::new(p),
                        Err(e) => {
                            eprintln!("{e}");
                            std::process::exit(2);
                        }
                    }
                }
                "anthropic" => {
                    match providers::anthropic::AnthropicProvider::from_env() {
                        Ok(p) => Box::new(p),
                        Err(e) => {
                            eprintln!("provider init: {e}");
                            std::process::exit(2);
                        }
                    }
                }
                other => {
                    eprintln!(
                        "unsupported provider: {other} (valid: openrouter, anthropic)"
                    );
                    std::process::exit(2);
                }
            }
        };

        let analytics_store = if cfg.analytics.enabled {
            let db_path = cfg.analytics.resolved_db_path();
            analytics::AnalyticsStore::new(&db_path, cfg.analytics.retention_days)
                .ok()
                .map(Arc::new)
        } else {
            None
        };
        let effective_model = model.unwrap_or_else(|| cfg.agent.model.clone());
        let approve_fn = if cfg.agent.approval_mode == "auto" {
            None
        } else {
            Some(safety::SafetyPolicy::stdin_approve_fn())
        };
        let args = agent_cmd::AgentCmdArgs {
            task,
            model: Some(effective_model),
            dry_run,
            safety: Some(cfg.agent.to_safety_policy(approve_fn)),
            analytics: analytics_store,
        };
        let result = agent_cmd::run_agent(llm.as_ref(), &workspace, args, &mut std::io::stdout()).await?;
        if result.stop_reason == providers::StopReason::Error {
            let msg = result.error_message.as_deref().unwrap_or("unknown error");
            eprintln!("agent error: {msg}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let startup_logs = startup_logs_early || cfg.mcp.startup_logs;
    // When MCP runs without startup diagnostics, avoid benign stderr lines —
    // Cursor surfaces subprocess stderr as `[error]` even for informational text.
    let mcp_quiet_stderr = cli.mcp && !startup_logs;

    script::configure_max_concurrent(cfg.process.max_script_threads);

    // Over-broad roots (a large directory with no project marker, e.g. $HOME
    // inherited as cwd) would index gigabytes of unrelated files and exhaust
    // the inotify watch cap. Detect once and suppress the heavyweight
    // watchers/indexers; the MCP `roots` handshake (or an explicit `-w`)
    // re-roots to a real project later.
    let overbroad = !index::should_eager_index(&workspace, &cfg.index);
    if overbroad && !mcp_quiet_stderr {
        eprintln!(
            "daimonos: workspace {:?} is an over-broad root (large, no project marker); \
             auto-index and file watching are suppressed. Launch with -w <project> \
             or let the client advertise MCP roots to index a real project.",
            workspace
        );
    }

    let ws_index = Arc::new(index::WorkspaceIndex::new(
        workspace.clone(),
        &cfg.index,
        !mcp_quiet_stderr,
    ));
    ws_index.spawn_reindex();

    // KGL startup auto-index (gated, best-effort): mirror the trigram index's
    // one-shot startup build so a fresh agent session gets a current graph
    // without a manual `kgl_query index`. Runs on a blocking task; never blocks
    // or breaks startup. Off unless DAIMONOS_KGL_AUTOINDEX is set.
    if kgl::autoindex::enabled() && !overbroad {
        let kgl_ws = workspace.clone();
        let quiet = mcp_quiet_stderr;
        let startup_cfg = cfg.kgl.clone();
        tokio::task::spawn_blocking(move || {
            let now = chrono::Utc::now().to_rfc3339();
            match kgl::autoindex::run_startup(&kgl_ws, &now, &startup_cfg) {
                Ok(Some((sub, nodes, edges))) => {
                    if !quiet {
                        eprintln!("kgl: startup index — {nodes} nodes / {edges} edges via {sub}");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    if !quiet {
                        eprintln!("kgl: startup index failed: {e}");
                    }
                }
            }
        });

        // A2: keep the graph fresh within the session via a debounced file watcher.
        kgl::autoindex::spawn_watcher(workspace.clone(), mcp_quiet_stderr, cfg.kgl.clone());
    }

    let tool_reg = Arc::new(tool_runner::ToolRegistry::new());
    config::register_tools(&cfg, &tool_reg, mcp_quiet_stderr).await;

    if plugins::git::is_available() {
        tool_reg
            .register(Arc::new(plugins::git::GitPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered git tool plugin");
        }
    }

    if plugins::docker::is_available() {
        tool_reg
            .register(Arc::new(plugins::docker::DockerPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered docker tool plugin");
        }
    }

    if plugins::cargo::is_available() {
        tool_reg
            .register(Arc::new(plugins::cargo::CargoPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered cargo tool plugin");
        }
    }

    if plugins::gh::is_available() {
        tool_reg
            .register(Arc::new(plugins::gh::GhPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered gh tool plugin");
        }
    }

    if plugins::pytest::is_available() {
        tool_reg
            .register(Arc::new(plugins::pytest::PytestPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered pytest tool plugin");
        }
    }

    if plugins::curl::is_available() {
        tool_reg
            .register(Arc::new(plugins::curl::CurlPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered curl tool plugin");
        }
    }

    if plugins::shellcheck::is_available() {
        tool_reg
            .register(Arc::new(plugins::shellcheck::ShellcheckPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered shellcheck tool plugin");
        }
    }

    if plugins::npm::is_available() {
        tool_reg
            .register(Arc::new(plugins::npm::NpmPlugin::new()))
            .await;
        if !mcp_quiet_stderr {
            eprintln!("auto-registered npm tool plugin");
        }
    }

    tool_reg
        .register(Arc::new(plugins::discord::DiscordPlugin::new(
            cfg.discord.clone(),
        )))
        .await;
    if !mcp_quiet_stderr {
        eprintln!("auto-registered discord tool plugin");
    }

    let pcache = Arc::new(pipeline_cache::PipelineCache::with_config_watching(
        &workspace,
        &cfg.pipeline_cache,
        !overbroad,
    ));

    // Analytics: --stats prints summary and exits
    if cli.stats {
        let db_path = cfg.analytics.resolved_db_path();
        if !db_path.exists() {
            eprintln!("No analytics data found at {}", db_path.display());
            return Ok(());
        }
        match analytics::AnalyticsStore::open_readonly(&db_path) {
            Ok(store) => {
                let report = store.format_stats_report_filtered(
                    cfg.analytics.retention_days,
                    cli.session_id.as_deref(),
                );
                eprint!("{report}");
            }
            Err(e) => eprintln!("Failed to open analytics: {e}"),
        }
        return Ok(());
    }

    // Initialize analytics store for session tracking
    let analytics_store = if cfg.analytics.enabled {
        let db_path = cfg.analytics.resolved_db_path();
        match analytics::AnalyticsStore::new(&db_path, cfg.analytics.retention_days) {
            Ok(store) => {
                if !mcp_quiet_stderr {
                    eprintln!("analytics: enabled ({})", db_path.display());
                }
                Some(Arc::new(store))
            }
            Err(e) => {
                eprintln!("analytics: disabled (init failed: {e})");
                None
            }
        }
    } else {
        None
    };

    if cli.mcp {
        mcp::run_mcp_server(
            workspace,
            cfg,
            ws_index,
            tool_reg,
            pcache,
            analytics_store,
            startup_logs,
        )
        .await
    } else if let Some(mcp_sock) = cli.mcp_socket {
        run_mcp_socket_server(
            mcp_sock,
            workspace,
            cfg,
            ws_index,
            tool_reg,
            pcache,
            analytics_store,
        )
        .await
    } else {
        run_socket_server(
            cli,
            workspace,
            cfg,
            ws_index,
            tool_reg,
            pcache,
            analytics_store,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_mcp_socket_server(
    sock_path: PathBuf,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    ws_index: Arc<index::WorkspaceIndex>,
    tool_reg: Arc<tool_runner::ToolRegistry>,
    pcache: Arc<pipeline_cache::PipelineCache>,
    analytics: Option<Arc<analytics::AnalyticsStore>>,
) -> anyhow::Result<()> {
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }

    let listener = UnixListener::bind(&sock_path)?;
    eprintln!(
        "daimonos MCP socket listening on {:?} (workspace: {:?})",
        sock_path, workspace
    );

    loop {
        let (stream, _addr) = listener.accept().await?;
        let ws = workspace.clone();
        let cfg_c = cfg.clone();
        let idx = ws_index.clone();
        let tr = tool_reg.clone();
        let pc = pcache.clone();
        let an = analytics.clone();

        tokio::spawn(async move {
            let mut session = session::Session::new(ws, cfg_c);
            session.index = Some(idx);
            session.tool_registry = Some(tr);
            session.pipeline_cache = Some(pc);
            session.analytics = an;
            session.external_session_id = analytics::read_agent_session_id_env();
            session.verbosity = config::effective_verbosity(&session.cfg);

            if let Err(e) = mcp::serve_one_mcp(stream, session).await {
                eprintln!("mcp socket connection error: {e}");
            }
        });
    }
}

async fn run_socket_server(
    cli: Cli,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    ws_index: Arc<index::WorkspaceIndex>,
    tool_reg: Arc<tool_runner::ToolRegistry>,
    pcache: Arc<pipeline_cache::PipelineCache>,
    analytics: Option<Arc<analytics::AnalyticsStore>>,
) -> anyhow::Result<()> {
    if cli.socket.exists() {
        std::fs::remove_file(&cli.socket)?;
    }

    eprintln!("pipeline cache: watching {:?}", workspace);

    let listener = UnixListener::bind(&cli.socket)?;
    eprintln!(
        "daimonos listening on {:?} (workspace: {:?})",
        cli.socket, workspace
    );

    loop {
        let (stream, _addr) = listener.accept().await?;
        let ws = workspace.clone();
        let debug = cli.debug;
        let idx = ws_index.clone();
        let cfg_clone = cfg.clone();
        let tr = tool_reg.clone();
        let pc = pcache.clone();
        let an = analytics.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ws, debug, idx, cfg_clone, tr, pc, an).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

// Per-connection handler threads the daemon's shared services (index, config,
// tool registry, pipeline cache, analytics) into the session; grouping them
// into a struct would only move the argument list elsewhere.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    workspace: PathBuf,
    debug: bool,
    ws_index: Arc<index::WorkspaceIndex>,
    cfg: Arc<config::Config>,
    tool_reg: Arc<tool_runner::ToolRegistry>,
    pcache: Arc<pipeline_cache::PipelineCache>,
    analytics: Option<Arc<analytics::AnalyticsStore>>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut session = session::Session::new(workspace, cfg);
    session.index = Some(ws_index);
    session.tool_registry = Some(tool_reg);
    session.pipeline_cache = Some(pcache);
    session.analytics = analytics;
    session.external_session_id = analytics::read_agent_session_id_env();
    session.verbosity = config::effective_verbosity(&session.cfg);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let response = match serde_json::from_str::<protocol::Request>(&line) {
            Ok(req) => ops::dispatch(&mut session, req).await,
            Err(e) => protocol::Response::err(3, &format!("parse error: {e}")),
        };

        let out = if debug {
            serde_json::to_string_pretty(&response)?
        } else {
            serde_json::to_string(&response)?
        };

        writer.write_all(out.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}
