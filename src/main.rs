mod acp_cmd;
mod agent;
mod agent_cmd;
mod agent_env;
mod agent_mcp;
mod agent_runtime;
mod analytics;
mod chat_cmd;
mod checkpoint;
mod cli;
mod client_transport;
mod compaction;
mod config;
mod context_metrics;
mod coordination;
mod env_file;
mod frontend_state;
mod headless_frontend;
mod herdr;
mod index;
mod kgl;
mod logging;
mod loop_detector;
mod managed_process;
mod mcp;
mod mcp_bridge;
mod observability;
mod ops;
mod paths;
mod pipeline_cache;
mod plugins;
mod prompts;
mod protocol;
mod providers;
mod provisioning;
mod remote_auth;
mod remote_gateway;
mod safety;
mod script;
mod session;
mod session_bootstrap;
mod session_catalog;
mod session_client;
mod session_controller;
mod session_core;
mod session_daemon;
mod session_factory;
mod session_interchange;
mod session_protocol;
mod session_store;
mod session_timeline;
mod skills;
mod snapshot;
mod tool_descriptions;
mod tool_facade;
mod tool_output;
mod tool_runner;
mod tools;
mod tui;
mod verbosity;
mod zed_config;

use clap::Parser;
use cli::{Cli, Command, RuntimeMode};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

struct DaemonOptions {
    socket: PathBuf,
    debug: bool,
}

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

/// Resolve the fixed `--debug-tokens` log path, creating its parent
/// directory if missing. Returns `None` (silently) if `$HOME` can't be
/// resolved or the directory can't be created — a debug log must not be
/// able to block startup.
fn debug_tokens_log_path() -> Option<PathBuf> {
    let home = paths::home_dir()?;
    let dir = home.join(".config/daimonos");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("token-debug.log"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let runtime_mode = cli.runtime_mode();

    if runtime_mode.is_mcp_stdio() {
        install_parent_death_signal();
    }

    session::enhance_process_path();

    let token_log = if cli.debug_tokens {
        debug_tokens_log_path()
    } else {
        None
    };

    let workspace = std::fs::canonicalize(&cli.workspace)?;

    // --print-config-path: report discovery order and which file wins, then
    // exit — before any config load / index / plugin setup.
    if cli.print_config_path {
        let candidates = config::search_candidates(cli.config.as_deref(), &workspace);
        println!("config: search order (first existing file wins):");
        let mut used: Option<&std::path::Path> = None;
        for (i, c) in candidates.iter().enumerate() {
            let found = c.is_file();
            if found && used.is_none() {
                used = Some(c);
            }
            let mark = if found { "found" } else { "not found" };
            println!("  {}. {} [{mark}]", i + 1, c.display());
        }
        match used {
            Some(p) => println!("=> using: {}", p.display()),
            None => println!("=> using: built-in defaults (no config file found)"),
        }
        return Ok(());
    }

    // --print-prompt <name>: dump one embedded baseline prompt to stdout, exit.
    if let Some(name) = cli.print_prompt.as_deref() {
        match prompts::default_by_name(name) {
            Some(text) => {
                print!("{text}");
                return Ok(());
            }
            None => {
                eprintln!(
                    "unknown prompt '{name}'. valid names: {}",
                    prompts::PROMPT_NAMES.join(", ")
                );
                std::process::exit(2);
            }
        }
    }

    // --dump-prompts [DIR]: scaffold all baseline prompts + a [prompts] block,
    // exit. `Some("")` means the flag was given with no value → default dir.
    if let Some(dir) = cli.dump_prompts.as_deref() {
        let dir_arg = if dir.is_empty() { None } else { Some(dir) };
        match prompts::dump_defaults(dir_arg, cli.force) {
            Ok(report) => {
                for name in &report.written {
                    println!(
                        "wrote {}/{}",
                        report.dir.display(),
                        prompts::prompt_filename(name)
                    );
                }
                for name in &report.skipped {
                    println!(
                        "skipped {}/{} (exists; use --force to overwrite)",
                        report.dir.display(),
                        prompts::prompt_filename(name)
                    );
                }
                println!(
                    "\nAdd this to your daimonos config to use these files (edit as needed):\n\n{}",
                    prompts::prompts_toml_block(&report.dir)
                );
                return Ok(());
            }
            Err(e) => {
                eprintln!("daimonos: --dump-prompts failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // Load an optional `agent.env` (dotenv-style) before config load and mode
    // dispatch, so every runtime mode and every later `std::env::var` read sees
    // the same values from one place. The real environment wins; loader-hijack
    // variables are refused. See `env_file`.
    env_file::load_default(&workspace, runtime_mode.is_mcp_stdio() && !cli.verbose);

    let startup_logs_early = cli.verbose || env_requests_mcp_startup_logs();
    let quiet_cfg_stderr = runtime_mode.is_mcp_stdio() && !startup_logs_early;
    let mut cfg = config::load(cli.config.as_deref(), &workspace, quiet_cfg_stderr);
    if let Err(e) = cfg.validate() {
        eprintln!(
            "config: validation error: {}",
            cfg.discord.redact_sensitive(&e)
        );
        std::process::exit(2);
    }
    cfg.prompts.resolved_tool_descriptions =
        tool_descriptions::ToolDescriptions::load(cfg.prompts.tool_descriptions.as_deref()).await;
    // Load optional extra user rules once, before any agent runtime starts.
    // Pure inspection modes (`agent --dry-run`, `chat --list`) do not create an
    // agent and therefore do not need the file.
    let uses_agent_prompt = match &cli.command {
        Some(Command::Agent(args)) => !args.dry_run,
        Some(Command::Chat(args)) => !args.list,
        Some(Command::Acp(_) | Command::SessionDaemon(_)) => true,
        Some(Command::Session(_) | Command::Mcp(_) | Command::Daemon) | None => false,
    };
    if uses_agent_prompt {
        cfg.prompts.additional_agent_instructions =
            match prompts::load_agent_instructions(cli.agent_instructions.as_deref(), &workspace)
                .await
            {
                Ok(instructions) => instructions,
                Err(e) => {
                    eprintln!("agent instructions: {e}");
                    std::process::exit(2);
                }
            };
    }
    let mut observability_config = cfg.observability.clone();
    let observability_ignored = observability_config.enabled && !uses_agent_prompt;
    if observability_ignored {
        observability_config.enabled = false;
    }
    let mut observability_runtime =
        observability::ObservabilityRuntime::initialize(&observability_config);
    let _logging_guard = match logging::init(&cfg.logging, observability_runtime.tracer()) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("logging: initialization failed: {error}");
            None
        }
    };
    if let observability::ObservabilityStatus::Failed(error) = observability_runtime.status() {
        if _logging_guard.is_some() {
            tracing::warn!(
                target: observability::LOCAL_DIAGNOSTIC_TARGET,
                event = "telemetry_initialization_failed",
                reason = %error,
            );
        } else {
            eprintln!("observability: initialization failed: {error}");
        }
    }
    if observability_ignored {
        if _logging_guard.is_some() {
            tracing::warn!(
                target: observability::LOCAL_DIAGNOSTIC_TARGET,
                event = "telemetry_ignored_for_runtime_mode",
                mode = runtime_mode.log_name(),
            );
        } else {
            eprintln!(
                "observability: ignored for runtime mode '{}'; supported modes: agent, chat, acp",
                runtime_mode.log_name()
            );
        }
    }
    let resource_telemetry = if cfg.logging.enabled && _logging_guard.is_some() {
        logging::spawn_resource_telemetry(cfg.logging.resource_interval_secs)
    } else {
        None
    };
    tracing::info!(
        target: "daimonos::lifecycle",
        event = "process_start",
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        mode = runtime_mode.log_name(),
        workspace = %workspace.display(),
        log_directory = %cfg.logging.resolved_directory().display(),
    );
    let cfg = Arc::new(cfg);
    let explicit_config_path = cli.config.clone();
    // Agent frontends skip workspace service setup; tool-serving modes share
    // the lower dispatcher. All paths rendezvous below for ordered telemetry
    // teardown before the logging guard is dropped.
    let result = match cli.command {
        Some(Command::Agent(args)) => {
            agent_runtime::run_agent(
                args,
                &workspace,
                Arc::clone(&cfg),
                token_log,
                explicit_config_path,
            )
            .await
        }
        Some(Command::Chat(args)) => {
            agent_runtime::run_chat(args, &workspace, Arc::clone(&cfg), token_log).await
        }
        Some(Command::Acp(args)) => {
            agent_runtime::run_acp(args, &workspace, Arc::clone(&cfg), token_log).await
        }
        Some(Command::SessionDaemon(args)) => {
            agent_runtime::run_session_daemon(args, &workspace, Arc::clone(&cfg), token_log).await
        }
        Some(Command::Session(args)) => session_interchange::run(args, &cfg),
        Some(Command::Mcp(_) | Command::Daemon) | None => {
            run_tool_service(
                runtime_mode,
                cli.socket,
                cli.debug,
                cli.session_id,
                workspace,
                cfg,
                startup_logs_early,
            )
            .await
        }
    };
    drop(resource_telemetry);
    observability_runtime.shutdown().await;
    result
}

/// Launch the tool-serving half of Daimonos. Agent, chat, and ACP modes return
/// before entering this function, so they cannot initialize workspace watchers,
/// native tool plugins, or protocol-daemon listeners by accident.
#[allow(clippy::too_many_arguments)]
async fn run_tool_service(
    runtime_mode: RuntimeMode,
    socket: PathBuf,
    debug: bool,
    session_id: Option<String>,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    startup_logs_early: bool,
) -> anyhow::Result<()> {
    let startup_logs = startup_logs_early || cfg.mcp.startup_logs;
    // When MCP runs without startup diagnostics, avoid benign stderr lines —
    // Cursor surfaces subprocess stderr as `[error]` even for informational text.
    let mcp_quiet_stderr = runtime_mode.is_mcp_stdio() && !startup_logs;

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

    // Analytics: --stats prints summary and exits
    if matches!(runtime_mode, RuntimeMode::Stats) {
        let db_path = cfg.analytics.resolved_db_path();
        if !db_path.exists() {
            eprintln!("No analytics data found at {}", db_path.display());
            return Ok(());
        }
        match analytics::AnalyticsStore::open_readonly(&db_path) {
            Ok(store) => {
                let report = store.format_stats_report_filtered(
                    cfg.analytics.retention_days,
                    session_id.as_deref(),
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

    let services = Arc::new(
        provisioning::build_tool_services(
            &workspace,
            &cfg,
            mcp_quiet_stderr,
            true,
            analytics_store,
        )
        .await,
    );

    match runtime_mode {
        RuntimeMode::McpStdio => mcp::run_mcp_server(workspace, cfg, services, startup_logs).await,
        RuntimeMode::McpSocket(mcp_sock) => {
            run_mcp_socket_server(mcp_sock, workspace, cfg, services).await
        }
        RuntimeMode::Daemon => {
            run_socket_server(DaemonOptions { socket, debug }, workspace, cfg, services).await
        }
        RuntimeMode::Agent
        | RuntimeMode::Chat
        | RuntimeMode::Acp
        | RuntimeMode::Session
        | RuntimeMode::SessionDaemon
        | RuntimeMode::Stats => {
            unreachable!("early-return runtime reached service dispatch")
        }
    }
}

async fn run_mcp_socket_server(
    sock_path: PathBuf,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    services: Arc<provisioning::ToolServices>,
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
        let services = Arc::clone(&services);

        tokio::spawn(async move {
            let mut session = session::Session::new(ws, cfg_c);
            provisioning::provision_session(&mut session, &services);

            if let Err(e) = mcp::serve_one_mcp(stream, session).await {
                eprintln!("mcp socket connection error: {e}");
            }
        });
    }
}

async fn run_socket_server(
    options: DaemonOptions,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    services: Arc<provisioning::ToolServices>,
) -> anyhow::Result<()> {
    if options.socket.exists() {
        std::fs::remove_file(&options.socket)?;
    }

    eprintln!("pipeline cache: watching {:?}", workspace);

    let listener = UnixListener::bind(&options.socket)?;
    eprintln!(
        "daimonos listening on {:?} (workspace: {:?})",
        options.socket, workspace
    );

    loop {
        let (stream, _addr) = listener.accept().await?;
        let ws = workspace.clone();
        let debug = options.debug;
        let cfg_clone = cfg.clone();
        let services = Arc::clone(&services);

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, ws, debug, cfg_clone, services).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    workspace: PathBuf,
    debug: bool,
    cfg: Arc<config::Config>,
    services: Arc<provisioning::ToolServices>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut session = session::Session::new(workspace, cfg);
    provisioning::provision_session(&mut session, &services);
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

    session.shutdown_processes().await;
    Ok(())
}
