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
mod confined_ignore;
mod context_metrics;
mod coordination;
mod env_file;
mod evidence;
mod frontend_state;
mod headless_frontend;
mod index;
mod kgl;
mod logging;
mod loop_detector;
mod managed_process;
mod mcp;
mod mcp_bridge;
mod mcp_config_cmd;
mod mcp_oauth;
mod observability;
mod ops;
mod path_policy;
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
mod state_root;
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

/// Internal listeners remain under caller-owned state when it is selected.
fn state_socket_path(
    authority: &state_root::StateAuthority,
    requested: &std::path::Path,
) -> Result<PathBuf, state_root::RunFailure> {
    let directory = authority.resolve(state_root::StateKey::Sockets, None)?;
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        directory.join(requested)
    };
    if candidate.file_name().is_none() {
        return Err(state_root::RunFailure::state_unusable(format!(
            "--socket {} has no file name",
            requested.display()
        )));
    }
    authority.explicit_destination("--socket", &candidate)
}

fn exit_for_state_failure(error: state_root::RunFailure) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({"error": {
            "code": "run_configuration_invalid", "reason": error.code, "message": error.message
        }})
    );
    std::process::exit(error.exit_code);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut runtime_mode = cli.runtime_mode();

    // ADR-021: until WP02 relocates frontend state, never accept a partial guarantee.
    if cli.state_dir.is_some()
        && !matches!(
            runtime_mode,
            RuntimeMode::McpStdio
                | RuntimeMode::McpSocket(_)
                | RuntimeMode::Daemon
                | RuntimeMode::Session
                | RuntimeMode::SessionDaemon
                | RuntimeMode::Acp
                | RuntimeMode::Chat
                | RuntimeMode::Agent
                | RuntimeMode::McpAuth
                | RuntimeMode::Stats
        )
    {
        eprintln!(
            "{}",
            serde_json::json!({"error":{
                "code":"run_configuration_invalid",
                "reason":"state_mode_unsupported",
                "message":"--state-dir is not supported for this runtime mode yet"
            }})
        );
        std::process::exit(2);
    }

    if runtime_mode.is_mcp_stdio() {
        install_parent_death_signal();
    }

    session::enhance_process_path();

    let workspace = match std::fs::canonicalize(&cli.workspace) {
        Ok(path) => path,
        Err(error) if cli.state_dir.is_some() => {
            eprintln!(
                "{}",
                serde_json::json!({"error": {
                    "code": "run_configuration_invalid", "reason": "state_unusable",
                    "message": format!("workspace {} cannot be resolved: {error}", cli.workspace.display())
                }})
            );
            std::process::exit(2);
        }
        Err(error) => return Err(error.into()),
    };
    let state_authority = match cli.state_dir.as_deref() {
        Some(path) => match state_root::StateAuthority::explicit(path, &workspace) {
            Ok(authority) => Some(authority),
            Err(error) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error":{"code":"run_configuration_invalid","reason":error.code,"message":error.message}})
                );
                std::process::exit(error.exit_code);
            }
        },
        None => None,
    };
    let caller_agent_env = std::env::var_os("DAIMONOS_AGENT_ENV").map(PathBuf::from);
    // Pin the confinement root before implicit startup file discovery (ADR-022).
    let mut filesystem_authority = if cli.confinement_requested() {
        let root = cli.confine_root.as_deref().unwrap_or(&workspace);
        match path_policy::FilesystemAuthority::confined(root, path_policy::FsLimits::default()) {
            Ok(authority) => Some(authority),
            Err(error) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": {
                        "code": error.code, "message": error.message
                    }})
                );
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    if filesystem_authority.is_some() && matches!(runtime_mode, RuntimeMode::McpConfig) {
        eprintln!(
            "{}",
            serde_json::json!({"error": {
                "code": path_policy::CODE_FILESYSTEM_POLICY_UNSUPPORTED,
                "message": "mcp-config local access cannot be confined"
            }})
        );
        std::process::exit(2);
    }
    let mut native_socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| PathBuf::from("/tmp/daimonos.sock"));
    if let Some(authority) = &state_authority {
        let requested = cli
            .socket
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("daemon.sock"));
        native_socket = match state_socket_path(authority, requested) {
            Ok(path) => path,
            Err(error) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error":{"code":"run_configuration_invalid","reason":error.code,"message":error.message}})
                );
                std::process::exit(error.exit_code);
            }
        };
        if let RuntimeMode::McpSocket(requested) = &runtime_mode {
            runtime_mode = match state_socket_path(authority, requested) {
                Ok(path) => RuntimeMode::McpSocket(path),
                Err(error) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({"error":{"code":"run_configuration_invalid","reason":error.code,"message":error.message}})
                    );
                    std::process::exit(error.exit_code);
                }
            };
        }
    }
    if filesystem_authority.is_some() && cli.dump_prompts.is_some() {
        eprintln!(
            "{}",
            serde_json::json!({"error": {
                "code": path_policy::CODE_FILESYSTEM_POLICY_UNSUPPORTED,
                "message": "--dump-prompts cannot be confined"
            }})
        );
        std::process::exit(2);
    }
    if state_authority.is_some()
        && cli
            .dump_prompts
            .as_ref()
            .is_some_and(|directory| directory.trim().is_empty())
    {
        exit_for_state_failure(state_root::RunFailure::state_unusable(
            "--dump-prompts requires an explicit output directory with --state-dir",
        ));
    }
    // Keep legacy behavior, but delay selected-mode writes until all config is validated.
    let mut token_log = if cli.debug_tokens && state_authority.is_none() {
        debug_tokens_log_path()
    } else {
        None
    };

    // --print-config-path: report discovery order and which file wins, then
    // exit — before any config load / index / plugin setup.
    if cli.print_config_path {
        let candidates = if state_authority.is_some() || filesystem_authority.is_some() {
            config::search_candidates_with_user_config(cli.config.as_deref(), &workspace, false)
        } else {
            config::search_candidates(cli.config.as_deref(), &workspace)
        };
        println!("config: search order (first existing file wins):");
        let mut used: Option<&std::path::Path> = None;
        for (i, c) in candidates.iter().enumerate() {
            let found = if let Some(authority) = &filesystem_authority {
                if cli.config.as_deref() == Some(c.as_path()) {
                    c.is_file()
                } else {
                    authority.open_read(c).is_ok()
                }
            } else {
                c.is_file()
            };
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
    if let Some(authority) = &filesystem_authority {
        env_file::load_confined(
            &workspace,
            authority,
            runtime_mode.is_mcp_stdio() && !cli.verbose,
        );
    } else if state_authority.is_some() {
        env_file::load_default_with_user_config(
            &workspace,
            runtime_mode.is_mcp_stdio() && !cli.verbose,
            false,
        );
    } else {
        env_file::load_default(&workspace, runtime_mode.is_mcp_stdio() && !cli.verbose);
    }

    let startup_logs_early = cli.verbose || env_requests_mcp_startup_logs();
    let quiet_cfg_stderr = runtime_mode.is_mcp_stdio() && !startup_logs_early;
    let mut cfg = if let Some(authority) = &filesystem_authority {
        match config::load_confined(
            cli.config.as_deref(),
            &workspace,
            quiet_cfg_stderr,
            authority,
        ) {
            Ok(cfg) => cfg,
            Err(message) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": {
                        "code": path_policy::CODE_FILESYSTEM_POLICY_UNSUPPORTED, "message": message
                    }})
                );
                std::process::exit(2);
            }
        }
    } else if state_authority.is_some() {
        config::load_with_user_config(cli.config.as_deref(), &workspace, quiet_cfg_stderr, false)
    } else {
        config::load(cli.config.as_deref(), &workspace, quiet_cfg_stderr)
    };
    if let Some(authority) = state_authority {
        if let Err(error) = cfg.apply_state_authority(authority, &workspace) {
            eprintln!(
                "{}",
                serde_json::json!({"error":{"code":"run_configuration_invalid","reason":error.code,"message":error.message}})
            );
            std::process::exit(error.exit_code);
        }
    }
    if let Err(e) = cfg.validate() {
        eprintln!(
            "config: validation error: {}",
            cfg.discord.redact_sensitive(&e)
        );
        std::process::exit(2);
    }
    if cli.confinement_requested() {
        if let Some(name) = cfg.tools.keys().min() {
            eprintln!(
                "{}",
                serde_json::json!({"error": {
                    "code": "filesystem_policy_unsupported",
                    "message": format!("configured local tool {name} cannot be confined")
                }})
            );
            std::process::exit(2);
        }
        if kgl::autoindex::enabled() || kgl::observe::enabled() {
            eprintln!(
                "{}",
                serde_json::json!({"error": {
                    "code": "filesystem_policy_unsupported",
                    "message": "KGL autoindex and observation are not supported with --confine-paths"
                }})
            );
            std::process::exit(2);
        }
        if let Some(mut authority) = filesystem_authority.take() {
            authority.set_limits(cfg.fs_limits());
            cfg.caller_agent_env = caller_agent_env;
            let authority = Arc::new(authority);
            cfg.agent.skills.filesystem_authority = Some(Arc::clone(&authority));
            cfg.acp.mcp.confined = true;
            cfg.filesystem_authority = Some(authority);
        }
    }
    if cfg.filesystem_authority.is_some()
        && cfg.state_authority.is_none()
        && matches!(&cli.command, Some(Command::Agent(args)) if args.debug_thoughts)
    {
        eprintln!(
            "{}",
            serde_json::json!({"error": {
                "code": "filesystem_policy_unsupported",
                "message": "--debug-thoughts requires --state-dir with --confine-paths"
            }})
        );
        std::process::exit(2);
    }
    if cfg.filesystem_authority.is_some()
        && cfg.state_authority.is_none()
        && !cfg.acp.mcp.oauth_servers.is_empty()
    {
        eprintln!(
            "{}",
            serde_json::json!({"error": {
                "code": "filesystem_policy_unsupported",
                "message": "OAuth credentials require --state-dir with --confine-paths"
            }})
        );
        std::process::exit(2);
    }
    if cfg.filesystem_authority.is_some()
        && cfg.state_authority.is_none()
        && std::env::var_os("DAIMONOS_OPENROUTER_TRACE_DIR").is_some_and(|value| !value.is_empty())
    {
        eprintln!(
            "{}",
            serde_json::json!({"error": {
                "code": "filesystem_policy_unsupported",
                "message": "DAIMONOS_OPENROUTER_TRACE_DIR requires --state-dir with --confine-paths"
            }})
        );
        std::process::exit(2);
    }
    if matches!(
        runtime_mode,
        RuntimeMode::Agent | RuntimeMode::Chat | RuntimeMode::Acp | RuntimeMode::SessionDaemon
    ) {
        if let Some(name) = agent_mcp::unconfineable_stdio(&cfg).await {
            eprintln!(
                "{}",
                serde_json::json!({"error": {
                    "code": "filesystem_policy_unsupported",
                    "message": format!("configured local MCP server {name} cannot be confined")
                }})
            );
            std::process::exit(2);
        }
    }
    if let (Some(authority), Some(Command::Session(args))) =
        (&cfg.filesystem_authority, &cli.command)
    {
        match &args.command {
            cli::SessionCommand::Import { file } => {
                let requested = if file.is_absolute() {
                    file.clone()
                } else {
                    std::env::current_dir()?.join(file)
                };
                if let Err(error) = authority.open_read(&requested) {
                    let error = error.with_argument(&file.to_string_lossy());
                    eprintln!("{}", serde_json::json!({"error": error}));
                    std::process::exit(2);
                }
            }
            cli::SessionCommand::Export {
                output: Some(_), ..
            } => {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": {
                        "code": path_policy::CODE_FILESYSTEM_POLICY_UNSUPPORTED,
                        "message": "session export --output cannot be confined"
                    }})
                );
                std::process::exit(2);
            }
            cli::SessionCommand::Export { output: None, .. } => {}
        }
    }
    // Validate enabled writer destinations after config validation and before any writer starts.
    if let Some(authority) = &cfg.state_authority {
        if cli.debug_tokens {
            let token_path = cfg.logging.resolved_directory().join("token-debug.log");
            let checked = authority
                .explicit_destination("--debug-tokens", &token_path)
                .and_then(|_| {
                    state_root::verify_unshared_file(&token_path).map_err(|error| {
                        state_root::RunFailure::state_unusable(format!(
                            "--debug-tokens {}: {error}",
                            token_path.display()
                        ))
                    })
                });
            checked.unwrap_or_else(|error| exit_for_state_failure(error));
        }
        if matches!(
            runtime_mode,
            RuntimeMode::Agent | RuntimeMode::Chat | RuntimeMode::Acp | RuntimeMode::SessionDaemon
        ) {
            if let Some(trace) =
                std::env::var_os("DAIMONOS_OPENROUTER_TRACE_DIR").filter(|value| !value.is_empty())
            {
                let directory = PathBuf::from(trace);
                let checked = authority
                    .explicit_destination("DAIMONOS_OPENROUTER_TRACE_DIR", &directory)
                    .and_then(|path| {
                        authority.verify_writable_directory("DAIMONOS_OPENROUTER_TRACE_DIR", &path)
                    });
                checked.unwrap_or_else(|error| exit_for_state_failure(error));
            }
        }
        if let Some(Command::Agent(args)) = &cli.command {
            if args.debug_thoughts {
                let path = args
                    .debug_thoughts_path
                    .clone()
                    .unwrap_or_else(|| cfg.logging.resolved_directory().join("thought-debug.log"));
                let checked = authority
                    .explicit_destination("--debug-thoughts-path", &path)
                    .and_then(|_| {
                        state_root::verify_unshared_file(&path).map_err(|error| {
                            state_root::RunFailure::state_unusable(format!(
                                "--debug-thoughts-path {}: {error}",
                                path.display()
                            ))
                        })
                    });
                checked.unwrap_or_else(|error| exit_for_state_failure(error));
            }
        }
        if matches!(
            runtime_mode,
            RuntimeMode::SessionDaemon | RuntimeMode::Agent
        ) {
            let socket = match &cli.command {
                Some(Command::SessionDaemon(args)) => args.socket.clone(),
                _ => None,
            }
            .unwrap_or_else(|| cfg.session.resolved_socket_path(&workspace));
            let checked = authority
                .explicit_destination("session-daemon --socket", &socket)
                .and_then(|_| {
                    let mut lock_name = socket.as_os_str().to_os_string();
                    lock_name.push(".lock");
                    let lock = PathBuf::from(lock_name);
                    state_root::verify_unshared_file(&lock).map_err(|error| {
                        state_root::RunFailure::state_unusable(format!(
                            "session socket lock {}: {error}",
                            lock.display()
                        ))
                    })
                });
            checked.unwrap_or_else(|error| exit_for_state_failure(error));
        }
        let session_kind = match runtime_mode {
            RuntimeMode::Session | RuntimeMode::SessionDaemon | RuntimeMode::Agent => {
                Some(state_root::SessionStoreKind::Daemon)
            }
            RuntimeMode::Acp => Some(state_root::SessionStoreKind::Acp),
            RuntimeMode::Chat => Some(state_root::SessionStoreKind::Chat),
            _ => None,
        };
        if let Some(kind) = session_kind {
            authority
                .verify_session_store(kind)
                .unwrap_or_else(|error| exit_for_state_failure(error));
        }
        if matches!(
            runtime_mode,
            RuntimeMode::SessionDaemon | RuntimeMode::Acp | RuntimeMode::Chat | RuntimeMode::Agent
        ) {
            authority
                .verify_approvals()
                .unwrap_or_else(|error| exit_for_state_failure(error));
        }
        let mut directories: Vec<(&str, PathBuf)> = Vec::new();
        if !cfg.acp.mcp.oauth_servers.is_empty() {
            if let Some(directory) = &cfg.acp.mcp.oauth_state_root {
                directories.push(("oauth", directory.clone()));
            }
        }
        if cfg.logging.enabled {
            directories.push(("logging.directory", cfg.logging.resolved_directory()));
        }
        if cfg.analytics.enabled {
            let database = cfg.analytics.resolved_db_path();
            if let Some(parent) = database.parent() {
                directories.push(("analytics.db_path", parent.to_path_buf()));
            }
        }
        if cfg.coordination.enabled {
            directories.push(("coordination.db_dir", cfg.coordination.resolved_db_dir()));
        }
        directories.push((
            "tool_output.directory",
            cfg.tool_output.resolved_directory(),
        ));
        if let Some(path) = &cfg.process.artifact_directory {
            directories.push(("process.artifact_directory", paths::expand_tilde(path)));
        }
        if let Some(path) = &cfg.git_read.status_directory {
            directories.push(("git_read.status_directory", paths::expand_tilde(path)));
        }
        if let Some(path) = &cfg.snapshot_directory {
            directories.push(("snapshot_directory", path.clone()));
        }
        if let Some(path) = &cfg.kgl.state_directory {
            directories.push(("kgl.state_directory", path.clone()));
        }
        let socket = match &runtime_mode {
            RuntimeMode::McpSocket(path) => Some(path.as_path()),
            RuntimeMode::Daemon => Some(native_socket.as_path()),
            _ => None,
        };
        if let Some(parent) = socket.and_then(std::path::Path::parent) {
            directories.push(("--socket", parent.to_path_buf()));
        }
        for (option, directory) in directories {
            if let Err(error) = authority.verify_writable_directory(option, &directory) {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": {
                        "code":"run_configuration_invalid", "reason":error.code, "message":error.message
                    }})
                );
                std::process::exit(error.exit_code);
            }
        }
    }
    if cli.debug_tokens {
        if let Some(authority) = &cfg.state_authority {
            let directory = cfg.logging.resolved_directory();
            authority.explicit_destination("--debug-tokens", &directory)?;
            std::fs::create_dir_all(&directory)?;
            token_log = Some(directory.join("token-debug.log"));
        }
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
        Some(Command::Session(_) | Command::Mcp(_) | Command::McpConfig(_) | Command::Daemon)
        | None => false,
    };
    if uses_agent_prompt {
        cfg.prompts.additional_agent_instructions = match if let Some(authority) =
            &cfg.filesystem_authority
        {
            prompts::load_agent_instructions_confined(
                cli.agent_instructions.as_deref(),
                &workspace,
                authority,
            )
            .await
        } else if cfg.state_authority.is_some() {
            prompts::load_agent_instructions_with_user_config(
                cli.agent_instructions.as_deref(),
                &workspace,
                false,
            )
            .await
        } else {
            prompts::load_agent_instructions(cli.agent_instructions.as_deref(), &workspace).await
        } {
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
    let _logging_guard = match logging::init(
        &cfg.logging,
        observability_runtime.tracer(),
        cfg.state_authority.is_some(),
    ) {
        Ok(guard) => guard,
        Err(error) => {
            if cfg.state_authority.is_some() && cfg.logging.enabled {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": {
                        "code": "run_configuration_invalid", "reason": "state_unusable",
                        "message": format!("logging.directory {}: {error}", cfg.logging.resolved_directory().display())
                    }})
                );
                std::process::exit(2);
            }
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
    let explicit_agent_instructions = cli.agent_instructions.clone();
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
                explicit_agent_instructions,
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
        Some(Command::McpConfig(args)) => mcp_config_cmd::run(args),
        Some(Command::Mcp(cli::McpArgs {
            command: Some(cli::McpCommand::Auth { command }),
            ..
        })) => mcp_oauth::run_command(command, &cfg.acp.mcp).await,
        Some(Command::Mcp(_) | Command::Daemon) | None => {
            run_tool_service(
                runtime_mode,
                native_socket,
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
    if cfg.state_authority.is_some() {
        let selected_socket = match &runtime_mode {
            RuntimeMode::McpSocket(path) => Some(path.as_path()),
            RuntimeMode::Daemon => Some(socket.as_path()),
            _ => None,
        };
        if let Some(path) = selected_socket {
            let directory = path.parent().ok_or_else(|| {
                state_root::RunFailure::state_unusable(format!(
                    "--socket {} has no parent",
                    path.display()
                ))
            })?;
            std::fs::create_dir_all(directory)?;
        }
    }
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
    if kgl::autoindex::enabled() && !overbroad && cfg.filesystem_authority.is_none() {
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
        match analytics::AnalyticsStore::open_async(&db_path, cfg.analytics.retention_days).await {
            Ok(store) => {
                if !mcp_quiet_stderr {
                    eprintln!("analytics: enabled ({})", db_path.display());
                }
                Some(Arc::new(store))
            }
            Err(e) => {
                if cfg.state_authority.is_some() {
                    eprintln!(
                        "{}",
                        serde_json::json!({"error": {
                            "code": "run_configuration_invalid", "reason": "state_unusable",
                            "message": format!("analytics.db_path {}: {e}", db_path.display())
                        }})
                    );
                    std::process::exit(2);
                }
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
        | RuntimeMode::McpConfig
        | RuntimeMode::McpAuth
        | RuntimeMode::Stats => {
            unreachable!("early-return runtime reached service dispatch")
        }
    }
}

/// Bind the explicit MCP socket without replacing an existing filesystem entry.
/// Stale socket recovery belongs to the lock-owning primary, not this mode.
fn bind_mcp_socket(sock_path: &std::path::Path) -> std::io::Result<UnixListener> {
    UnixListener::bind(sock_path)
}

async fn run_mcp_socket_server(
    sock_path: PathBuf,
    workspace: PathBuf,
    cfg: Arc<config::Config>,
    services: Arc<provisioning::ToolServices>,
) -> anyhow::Result<()> {
    // Never unlink an existing path here: it may be a live server's socket,
    // or even a symlink. A shared primary must arbitrate stale sockets under
    // its per-workspace lock; the explicit --mcp-socket mode owns no such lock.
    let listener = bind_mcp_socket(&sock_path)?;
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

#[cfg(test)]
mod mcp_socket_bind_tests {
    use super::bind_mcp_socket;

    #[tokio::test]
    async fn second_bind_preserves_live_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.sock");
        let first = bind_mcp_socket(&path).unwrap();
        assert!(bind_mcp_socket(&path).is_err());
        assert!(path.exists());

        // The original listener must remain reachable at the same path.
        let client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (_accepted, _) = first.accept().await.unwrap();
        drop(client);
    }

    #[tokio::test]
    async fn stale_socket_is_not_removed_by_explicit_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        let listener = bind_mcp_socket(&path).unwrap();
        drop(listener);
        assert!(bind_mcp_socket(&path).is_err());
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_does_not_unlink_symlink_or_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("mcp.sock");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(bind_mcp_socket(&link).is_err());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    }
}
