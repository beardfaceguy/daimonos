use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Stdout format of a one-shot `agent` run (ADR-036).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum AgentOutputFormat {
    /// The assistant text, as before ADR-036.
    #[default]
    Text,
    /// One JSON result document and one newline.
    Json,
}

#[derive(Debug, Args)]
pub struct AgentArgs {
    /// Optional initial task. Required unless --interactive launches on a TTY.
    pub task: Option<String>,
    /// Launch the full-screen terminal UI when stdin and stdout are terminals.
    #[arg(short = 'i', long, default_value_t = false)]
    pub interactive: bool,
    /// Render the interactive TUI without terminal colors.
    #[arg(
        long,
        default_value_t = false,
        requires = "interactive",
        conflicts_with = "print"
    )]
    pub no_color: bool,
    /// Force stable one-shot output, including when --interactive is also set.
    #[arg(long, default_value_t = false)]
    pub print: bool,
    /// Model override (default: from the agent env file)
    #[arg(long)]
    pub model: Option<String>,
    /// LLM provider override (default: from the agent env file)
    #[arg(long)]
    pub provider: Option<String>,
    /// Print available tools and task without calling the API
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Path to the agent env file (default: $DAIMONOS_AGENT_ENV or
    /// ~/.config/daimonos/agent.env)
    #[arg(long)]
    pub agent_env: Option<PathBuf>,
    /// Persist streamed model thinking to a private local debug file.
    #[arg(long, default_value_t = false, conflicts_with = "interactive")]
    pub debug_thoughts: bool,
    /// Override the --debug-thoughts destination (default:
    /// ~/.config/daimonos/thought-debug.log). The file is truncated per run.
    #[arg(
        long,
        value_name = "PATH",
        requires = "debug_thoughts",
        conflicts_with = "interactive"
    )]
    pub debug_thoughts_path: Option<PathBuf>,
    /// WP07 (FR-04): opt-in controlled model transport. Path to ONE JSON object
    /// owning the five required values (`provider_label`, `model`, `base_url`,
    /// `wire` = "chat"|"messages", `credential_env`) and the optional controlled
    /// request settings (`max_tokens`, `timeout_ms`, `request_retries`, `proxy`,
    /// `extra_body_fields`). The sole source for those values; cannot be combined
    /// with --model/--provider. Default off keeps legacy provider behavior.
    #[arg(
        long,
        value_name = "PATH",
        // WP07 R1 (review r3): the controlled profile is honored only by the
        // one-shot print send-path. Interactive mode returns through the session
        // daemon, which carries no profile, so it could only fall back to the
        // legacy transport. A security-critical opt-in must fail closed: refuse
        // the combination at parse time. Interactive mode is reachable only via
        // --interactive (tui::resolve_agent_mode), so this closes every path.
        conflicts_with_all = ["model", "provider", "interactive"]
    )]
    pub model_profile: Option<PathBuf>,
    /// Opt-in structured final output (ADR-024, FR-01). Path to a JSON control
    /// file: `schema` XOR `schema_path`, `tool_name`, `tool_description`,
    /// `corrective_retries`, and optional `max_model_requests`. On success the
    /// run emits only the validated argument JSON and one newline on stdout; a
    /// failed ending emits the typed final-output error. Default off.
    ///
    /// Honored only by the one-shot `--print` path. Interactive and dry-run
    /// modes cannot emit a single validated terminal document, so the flag is
    /// refused there at startup (ADR-024 / INTERFACES.md frontend coverage)
    /// rather than silently ignored. The chat, ACP, and session-daemon
    /// frontends never carry this agent-only flag at all.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["interactive", "dry_run"])]
    pub final_output: Option<PathBuf>,
    /// WP09 (FR-04 requirements 14-17, ADR-027): opt-in same-tool capability
    /// probe. Path to a JSON file holding ONE minimal valid payload for the
    /// final-answer tool. Before any evidence tool runs, Daimonos sends exactly
    /// one unretried probe offering the frozen run tool catalog and asking the
    /// model to call the final-answer tool once with this payload. Only one
    /// schema-valid final call passes; any other outcome exits 21
    /// (`model_unsupported`) and does no further work. Requires --final-output
    /// (FR-01) and --model-profile (controlled transport); omitting either is a
    /// startup refusal (exit 2). Default off.
    #[arg(long, value_name = "PATH")]
    pub capability_probe_payload: Option<PathBuf>,
    /// WP10 (FR-05, ADR-028): opt-in credential-free worker execution. Path to
    /// a JSON control file of explicit MCP-stdio worker definitions and their
    /// explicit, credential-free child environment (`workers` map of
    /// name -> {command, args, env}). When set, every non-final tool call is
    /// routed to a caller-supplied worker or refused; nothing runs in the
    /// model-side process, and no credential, provider config, or inherited
    /// environment reaches a worker. The final-answer tool (FR-01) is validated
    /// and emitted model-side and is never forwarded to a worker. Default off.
    ///
    /// Honored only by the one-shot `--print` path. Interactive and dry-run
    /// modes cannot host the two-process split (interactive returns through the
    /// worker-less session daemon; dry-run never runs the loop), so the flag is
    /// refused there at startup rather than silently ignored. The chat, ACP,
    /// and session-daemon frontends never carry this agent-only flag at all.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["interactive", "dry_run"])]
    pub worker_config: Option<PathBuf>,
    /// One-shot stdout format. `json` prints one result document for each run
    /// whose loop returned a result, including failed runs (ADR-036).
    #[arg(
        long,
        value_enum,
        default_value_t = AgentOutputFormat::Text,
        // `final_output`: ADR-036 Decision 14 (the document carries the
        // validated final output) is not built. Until then the combination
        // is refused, because both options own stdout.
        // `capability_probe_payload`: a probe requires `--final-output`, so
        // it is refused here by name and not only through that rule.
        conflicts_with_all = [
            "interactive",
            "dry_run",
            "final_output",
            "capability_probe_payload"
        ]
    )]
    pub output: AgentOutputFormat,
    /// Start a named one-shot session and save it when the run ends. A later
    /// run continues it with --resume. An existing id is refused (ADR-036).
    /// This is not the top-level --session-id, which filters --stats.
    #[arg(
        long = "session-id",
        value_name = "ID",
        // A session with structured final output, a controlled model profile,
        // or workers is not built or tested: the prefix record does not hold
        // the endpoint of a profile. The combination is refused.
        conflicts_with_all = [
            "interactive",
            "dry_run",
            "final_output",
            "model_profile",
            "worker_config"
        ]
    )]
    pub oneshot_session_id: Option<String>,
    /// Continue a named one-shot session with the task as the new input. The
    /// run uses the saved provider, model, and thinking level; a different
    /// --model or provider is refused (ADR-036).
    #[arg(
        long,
        value_name = "ID",
        conflicts_with_all = [
            "interactive",
            "dry_run",
            "oneshot_session_id",
            "final_output",
            "model_profile",
            "worker_config"
        ]
    )]
    pub resume: Option<String>,
}

#[derive(Debug, Args)]
pub struct ChatArgs {
    /// Model override (default: from the agent env file)
    #[arg(long)]
    pub model: Option<String>,
    /// LLM provider override (default: from the agent env file)
    #[arg(long)]
    pub provider: Option<String>,
    /// Path to the agent env file (default: $DAIMONOS_AGENT_ENV or
    /// ~/.config/daimonos/agent.env)
    #[arg(long)]
    pub agent_env: Option<PathBuf>,
    /// Resume a saved chat session by id (see --list)
    #[arg(long)]
    pub resume: Option<String>,
    /// List saved chat sessions and exit
    #[arg(long, default_value_t = false)]
    pub list: bool,
}

#[derive(Debug, Args)]
pub struct AcpArgs {
    /// Model override (default: from the agent env file)
    #[arg(long)]
    pub model: Option<String>,
    /// LLM provider override (default: from the agent env file)
    #[arg(long)]
    pub provider: Option<String>,
    /// Path to the agent env file (default: $DAIMONOS_AGENT_ENV or
    /// ~/.config/daimonos/agent.env)
    #[arg(long)]
    pub agent_env: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SessionDaemonArgs {
    /// Local Unix socket override (default: session.socket_path).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Model override (default: from the agent env file).
    #[arg(long)]
    pub model: Option<String>,
    /// LLM provider override (default: from the agent env file).
    #[arg(long)]
    pub provider: Option<String>,
    /// Path to the agent env file.
    #[arg(long)]
    pub agent_env: Option<PathBuf>,
    /// Explicitly enable the remote WebSocket gateway on this loopback address.
    #[arg(long, value_name = "IP:PORT")]
    pub remote_listen: Option<std::net::SocketAddr>,
    /// Browser Origin allowed to open the remote WebSocket. Repeat as needed.
    #[arg(long = "remote-origin", value_name = "ORIGIN")]
    pub remote_origins: Vec<String>,
    /// Permit local consent to grant remote AllowAlways.
    #[arg(long, default_value_t = false)]
    pub remote_allow_always: bool,
    /// Trust X-Forwarded-For only from the loopback reverse proxy.
    #[arg(long, default_value_t = false)]
    pub remote_trust_proxy_headers: bool,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    /// Serve MCP over this Unix socket instead of stdio.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<McpCommand>,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Authorize, inspect, or remove a Daimonos-owned OAuth grant.
    Auth {
        #[command(subcommand)]
        command: McpAuthCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpAuthCommand {
    /// Authorize a server in the browser, displaying the URL as a fallback.
    Login { server: String },
    /// Show nonsecret grant status for a configured server.
    Status { server: String },
    /// Remove a Daimonos-owned grant; does not touch Zed's keychain.
    Logout { server: String },
}

#[derive(Debug, Args)]
pub struct McpConfigArgs {
    #[command(subcommand)]
    pub command: McpConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum McpConfigCommand {
    /// Translate Zed context_servers into a harness mcpServers file.
    Sync {
        /// Zed settings.json source (defaults to the platform Zed path).
        #[arg(long, value_name = "FILE")]
        source: Option<PathBuf>,
        /// Harness MCP file to replace atomically.
        #[arg(long, value_name = "FILE", default_value = "~/.cursor/mcp.json")]
        target: PathBuf,
        /// Print the generated document without writing it.
        #[arg(long)]
        dry_run: bool,
        /// After a successful sync, replace this process with COMMAND.
        #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
        exec: Vec<String>,
    },
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Import one versioned session archive; duplicate session ids are rejected.
    Import {
        /// JSON session archive to import.
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Export one persisted session.
    Export {
        /// Persisted session id.
        session_id: String,
        /// Archive format.
        #[arg(long, default_value = "json")]
        format: String,
        /// Write to a new file instead of stdout.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the agent once, or launch the opt-in interactive terminal UI.
    Agent(AgentArgs),
    /// Start an interactive chat REPL over a stateful agent session.
    Chat(ChatArgs),
    /// Run a native Agent Client Protocol engine over stdio.
    Acp(AcpArgs),
    /// Run the persistent local interactive-session daemon.
    SessionDaemon(SessionDaemonArgs),
    /// Import or export persisted agent sessions.
    Session(SessionArgs),
    /// Run the MCP tool server over stdio or a Unix socket.
    Mcp(McpArgs),
    /// Synchronize Zed's MCP servers into harness-specific configuration.
    McpConfig(McpConfigArgs),
    /// Run the compact opcode protocol daemon over a Unix socket.
    Daemon,
    /// Print the effective tool contract as deterministic JSON and exit
    /// (WP15, FR-13). Session-free: no model run or session store is started.
    ToolContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeMode {
    Agent,
    Chat,
    Acp,
    SessionDaemon,
    Session,
    McpStdio,
    McpSocket(PathBuf),
    McpConfig,
    McpAuth,
    Daemon,
    Stats,
    ToolContract,
}

impl RuntimeMode {
    pub fn log_name(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Chat => "chat",
            Self::Acp => "acp",
            Self::SessionDaemon => "session_daemon",
            Self::Session => "session",
            Self::McpStdio => "mcp_stdio",
            Self::McpSocket(_) => "mcp_socket",
            Self::McpConfig => "mcp_config",
            Self::McpAuth => "mcp_auth",
            Self::Daemon => "socket",
            Self::Stats => "stats",
            Self::ToolContract => "tool_contract",
        }
    }

    pub fn is_mcp_stdio(&self) -> bool {
        matches!(self, Self::McpStdio)
    }
}

#[derive(Debug, Parser)]
#[command(name = "daimonos", about = "Daimonos — agent-optimized OS layer")]
pub struct Cli {
    /// Unix socket path used by `daemon` and the legacy default mode (default: /tmp/daimonos.sock).
    #[arg(short, long)]
    pub socket: Option<PathBuf>,

    /// Workspace root directory.
    #[arg(short, long, default_value = ".")]
    pub workspace: PathBuf,

    /// Private directory for internal state. Context: ADR-021.
    #[arg(long, value_name = "PATH", global = true)]
    pub state_dir: Option<PathBuf>,

    /// Human-readable debug output (daemon mode only).
    #[arg(long, default_value_t = false)]
    pub debug: bool,

    /// Path to config file (default: search workspace then ~/.config/daimonos/).
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Legacy alias for `daimonos mcp`.
    #[arg(long, default_value_t = false, conflicts_with = "mcp_socket")]
    pub mcp: bool,

    /// Legacy alias for `daimonos mcp --socket <PATH>`.
    #[arg(long, conflicts_with = "mcp")]
    pub mcp_socket: Option<PathBuf>,

    /// Emit informational stderr during MCP startup.
    #[arg(long, default_value_t = false)]
    pub verbose: bool,

    /// Print token analytics summary and exit.
    #[arg(long, default_value_t = false)]
    pub stats: bool,

    /// Print the config file search order and exit.
    #[arg(long, default_value_t = false)]
    pub print_config_path: bool,

    /// Print one embedded baseline prompt and exit.
    #[arg(long, value_name = "NAME")]
    pub print_prompt: Option<String>,

    /// Write all embedded baseline prompts to a directory and exit.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "")]
    pub dump_prompts: Option<String>,

    /// Overwrite existing files with `--dump-prompts`.
    #[arg(long, default_value_t = false, requires = "dump_prompts")]
    pub force: bool,

    /// Additional user instructions appended to agent prompts.
    #[arg(long, global = true, value_name = "PATH")]
    pub agent_instructions: Option<PathBuf>,

    /// Restrict `--stats` to one external agent session id.
    #[arg(long)]
    pub session_id: Option<String>,

    /// Log per-LLM-call token usage for agent and chat modes.
    #[arg(long, default_value_t = false)]
    pub debug_tokens: bool,

    /// Confine local filesystem access to a fixed root (ADR-022).
    #[arg(long, global = true, default_value_t = false)]
    pub confine_paths: bool,

    /// Set the confinement root. Defaults to the workspace. An explicit root
    /// also enables confinement (reviewer-runtime INTERFACES.md, FR-03).
    #[arg(long, global = true, value_name = "PATH")]
    pub confine_root: Option<PathBuf>,

    /// Prohibit every workspace mutation end to end (FR-07, ADR-022). Internal
    /// state still writes under `--state-dir`. Default off; when absent,
    /// behavior is unchanged.
    #[arg(long, global = true, default_value_t = false)]
    pub workspace_read_only: bool,

    /// Write a caller-requested, private JSONL run audit to PATH (WP08, FR-04).
    /// Default off. The destination is outside evidence and, with --state-dir,
    /// must be under the state root. A requested audit write failure stops the
    /// run (exit 23). Context: ADR-026.
    #[arg(long, global = true, value_name = "PATH")]
    pub audit_path: Option<PathBuf>,

    /// Enforce an explicit tool allowlist for the run (FR-02, ADR-023). PATH is
    /// a JSON array of exact exported tool names. Omitted keeps legacy
    /// behavior (all tools). An empty array `[]` grants no tools. Unknown names
    /// fail at startup (exit 2). The list is the effective set at every model
    /// listing and every direct, batch, Starlark, plugin, and remote dispatch;
    /// it cannot be widened by workspace/environment config. Default off.
    #[arg(long, global = true, value_name = "PATH")]
    pub tool_allowlist: Option<PathBuf>,

    /// Stop an opted-in tool call at an absolute per-call deadline of N
    /// milliseconds (WP18, FR-19, ADR-031). External work (exec, git_read)
    /// inherits the remaining deadline and its process group is reaped. A
    /// deadline hit returns `tool_timeout` with no partial result. Positive
    /// integer; omitted means no new limit; default off.
    #[arg(long = "tool-timeout-ms", global = true, value_name = "N")]
    pub tool_timeout_ms: Option<u64>,

    /// Refuse an opted-in tool call once its logical input exceeds N bytes
    /// (WP18, FR-19, ADR-031). Counts file/git content read or scanned,
    /// including cached-index content — not displayed output, which is a
    /// separate truncation mechanism. A hit returns `tool_input_limit`.
    /// Positive integer; omitted means no new limit; default off.
    #[arg(long = "tool-input-bytes", global = true, value_name = "N")]
    pub tool_input_bytes: Option<u64>,

    /// Refuse an opted-in tool call once it walks more than N directory
    /// entries (WP18, FR-19, ADR-031; integrator D56 `(proposed)`). A hit
    /// returns `tool_input_limit` naming the entry ceiling. Positive integer;
    /// omitted means no new limit; default off.
    #[arg(long = "tool-max-entries", global = true, value_name = "N")]
    pub tool_max_entries: Option<u64>,

    /// Serve and export full, self-contained JSON Schema draft 2020-12 tool
    /// schemas instead of the default stripped placeholders (WP17, FR-15).
    /// Equivalent to `[mcp] full_tool_schemas = true`; the config/env seam still
    /// applies. Opt-in: the legacy terse/lazy serving stays the default when it
    /// is absent. Context: docs/adr/030-self-contained-full-schemas.md.
    #[arg(long, global = true, default_value_t = false)]
    pub full_tool_schemas: bool,

    /// Refuse every implicit configuration source (environment files, config,
    /// instructions, skills, registrations, ignore files); honour only explicit
    /// caller inputs such as `--config` (reviewer-runtime INTERFACES.md, FR-09;
    /// ADR-021). Default-off: current discovery behaviour is unchanged.
    #[arg(long, global = true, default_value_t = false)]
    pub ignore_config_files: bool,

    /// Bound oversized tool output in place without writing a full-output spill
    /// copy or emitting a saved-file pointer (ADR-021, FR-11). Truncated results
    /// keep paging/narrowing guidance. Default behavior is preserved when absent.
    #[arg(long, global = true, default_value_t = false)]
    pub no_output_spill: bool,

    /// Report symbolic links without following them. Independent of
    /// confinement; both may be combined (ADR-022, WP14, FR-12).
    #[arg(long, global = true, default_value_t = false)]
    pub no_follow_symlinks: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// True when either confinement flag is present (ADR-022).
    pub fn confinement_requested(&self) -> bool {
        self.confine_paths || self.confine_root.is_some()
    }

    /// True when explicit-only configuration discovery is requested (FR-09).
    pub fn explicit_only(&self) -> bool {
        self.ignore_config_files
    }

    /// True when the shared filesystem authority must be constructed: either
    /// confinement or the independent no-follow option is requested (ADR-022,
    /// WP14). No-follow is enforced through the same pinned-root authority, so
    /// the authority is built even when confinement is absent.
    pub fn filesystem_authority_requested(&self) -> bool {
        self.confinement_requested() || self.no_follow_symlinks
    }

    /// Resolve old flags and normalized subcommands to one runtime identity.
    /// Explicit subcommands retain the historical precedence over legacy flags.
    pub fn runtime_mode(&self) -> RuntimeMode {
        match &self.command {
            Some(Command::Agent(_)) => RuntimeMode::Agent,
            Some(Command::Chat(_)) => RuntimeMode::Chat,
            Some(Command::Acp(_)) => RuntimeMode::Acp,
            Some(Command::SessionDaemon(_)) => RuntimeMode::SessionDaemon,
            Some(Command::Session(_)) => RuntimeMode::Session,
            Some(Command::McpConfig(_)) => RuntimeMode::McpConfig,
            Some(Command::Mcp(args)) if args.command.is_some() => RuntimeMode::McpAuth,
            Some(Command::Mcp(args)) => args
                .socket
                .clone()
                .map(RuntimeMode::McpSocket)
                .unwrap_or(RuntimeMode::McpStdio),
            Some(Command::Daemon) => RuntimeMode::Daemon,
            Some(Command::ToolContract) => RuntimeMode::ToolContract,
            None if self.stats => RuntimeMode::Stats,
            None if self.mcp => RuntimeMode::McpStdio,
            None => self
                .mcp_socket
                .clone()
                .map(RuntimeMode::McpSocket)
                .unwrap_or(RuntimeMode::Daemon),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_config_files_is_global_and_default_off() {
        assert!(!Cli::try_parse_from(["daimonos", "mcp"])
            .unwrap()
            .explicit_only());
        assert!(
            Cli::try_parse_from(["daimonos", "--ignore-config-files", "mcp"])
                .unwrap()
                .explicit_only()
        );
        assert!(
            Cli::try_parse_from(["daimonos", "mcp", "--ignore-config-files"])
                .unwrap()
                .explicit_only()
        );
    }

    #[test]
    fn confinement_flags_are_global_and_root_implies_confinement() {
        assert!(Cli::try_parse_from(["daimonos", "--confine-paths", "acp"]).is_ok());
        assert!(Cli::try_parse_from(["daimonos", "acp", "--confine-paths"]).is_ok());
        let cli = Cli::try_parse_from(["daimonos", "--confine-root", "/tmp", "acp"]).unwrap();
        assert!(!cli.confine_paths);
        assert!(cli.confinement_requested());
        assert!(!Cli::try_parse_from(["daimonos", "acp"])
            .unwrap()
            .confinement_requested());
    }

    #[test]
    fn no_output_spill_is_a_global_opt_in_flag() {
        // Global: parses before and after a subcommand, and defaults off.
        assert!(
            !Cli::try_parse_from(["daimonos", "mcp"])
                .unwrap()
                .no_output_spill
        );
        assert!(
            Cli::try_parse_from(["daimonos", "--no-output-spill", "mcp"])
                .unwrap()
                .no_output_spill
        );
        assert!(
            Cli::try_parse_from(["daimonos", "mcp", "--no-output-spill"])
                .unwrap()
                .no_output_spill
        );
    }

    #[test]
    fn accepts_explicit_state_directory_before_and_after_subcommand() {
        assert!(Cli::try_parse_from(["daimonos", "--state-dir", "/private/state", "mcp"]).is_ok());
        assert!(Cli::try_parse_from(["daimonos", "mcp", "--state-dir", "/private/state"]).is_ok());
    }

    fn mode(args: &[&str]) -> RuntimeMode {
        Cli::try_parse_from(args).unwrap().runtime_mode()
    }

    #[test]
    fn normalized_and_legacy_mcp_stdio_are_equivalent() {
        assert_eq!(mode(&["daimonos", "mcp"]), RuntimeMode::McpStdio);
        assert_eq!(mode(&["daimonos", "--mcp"]), RuntimeMode::McpStdio);
    }

    #[test]
    fn normalized_and_legacy_mcp_socket_are_equivalent() {
        let expected = RuntimeMode::McpSocket(PathBuf::from("/tmp/mcp.sock"));
        assert_eq!(
            mode(&["daimonos", "mcp", "--socket", "/tmp/mcp.sock"]),
            expected
        );
        assert_eq!(
            mode(&["daimonos", "--mcp-socket", "/tmp/mcp.sock"]),
            expected
        );
    }

    #[test]
    fn normalized_and_legacy_daemon_are_equivalent() {
        assert_eq!(mode(&["daimonos", "daemon"]), RuntimeMode::Daemon);
        assert_eq!(mode(&["daimonos"]), RuntimeMode::Daemon);
    }

    #[test]
    fn agent_family_and_stats_resolve_distinct_modes() {
        assert_eq!(
            mode(&["daimonos", "agent", "do work", "--dry-run"]),
            RuntimeMode::Agent
        );
        assert_eq!(mode(&["daimonos", "chat", "--list"]), RuntimeMode::Chat);
        assert_eq!(mode(&["daimonos", "acp"]), RuntimeMode::Acp);
        assert_eq!(
            mode(&["daimonos", "session-daemon"]),
            RuntimeMode::SessionDaemon
        );
        assert_eq!(
            mode(&["daimonos", "session", "import", "session.json"]),
            RuntimeMode::Session
        );
        assert_eq!(mode(&["daimonos", "--stats"]), RuntimeMode::Stats);
    }

    #[test]
    fn session_export_accepts_format_and_output() {
        let cli = Cli::try_parse_from([
            "daimonos",
            "session",
            "export",
            "session-1",
            "--format",
            "json",
            "--output",
            "session.json",
        ])
        .unwrap();
        let Some(Command::Session(SessionArgs {
            command:
                SessionCommand::Export {
                    session_id,
                    format,
                    output,
                },
        })) = cli.command
        else {
            panic!("session export command");
        };
        assert_eq!(session_id, "session-1");
        assert_eq!(format, "json");
        assert_eq!(output, Some(PathBuf::from("session.json")));
    }

    #[test]
    fn interactive_agent_accepts_no_initial_task() {
        let cli = Cli::try_parse_from(["daimonos", "agent", "--interactive"])
            .expect("interactive agent should allow an empty composer");
        let Some(Command::Agent(args)) = cli.command else {
            panic!("agent command");
        };

        assert!(args.interactive);
        assert!(!args.print);
        assert!(args.task.is_none());
    }

    #[test]
    fn no_color_is_scoped_to_interactive_agent_mode() {
        let cli = Cli::try_parse_from(["daimonos", "agent", "--interactive", "--no-color"])
            .expect("interactive no-color mode");
        let Some(Command::Agent(args)) = cli.command else {
            panic!("agent command");
        };
        assert!(args.no_color);

        assert!(
            Cli::try_parse_from(["daimonos", "agent", "--no-color", "do work"]).is_err(),
            "--no-color without --interactive should be rejected"
        );
        assert!(Cli::try_parse_from([
            "daimonos",
            "agent",
            "--interactive",
            "--print",
            "--no-color",
            "do work",
        ])
        .is_err());
    }

    #[test]
    fn print_agent_keeps_optional_task_for_runtime_validation() {
        let cli = Cli::try_parse_from(["daimonos", "agent", "--print", "do work"])
            .expect("explicit print mode");
        let Some(Command::Agent(args)) = cli.command else {
            panic!("agent command");
        };

        assert!(args.print);
        assert!(!args.interactive);
        assert_eq!(args.task.as_deref(), Some("do work"));
    }

    #[test]
    fn debug_thought_capture_requires_opt_in_and_accepts_path_override() {
        let cli = Cli::try_parse_from([
            "daimonos",
            "agent",
            "--debug-thoughts",
            "--debug-thoughts-path",
            "/tmp/thoughts.log",
            "do work",
        ])
        .expect("explicit thought capture");
        let Some(Command::Agent(args)) = cli.command else {
            panic!("agent command");
        };
        assert!(args.debug_thoughts);
        assert_eq!(
            args.debug_thoughts_path.as_deref(),
            Some(std::path::Path::new("/tmp/thoughts.log"))
        );
        assert!(Cli::try_parse_from([
            "daimonos",
            "agent",
            "--debug-thoughts-path",
            "/tmp/thoughts.log",
            "do work",
        ])
        .is_err());
    }

    #[test]
    fn explicit_subcommand_preserves_precedence_over_legacy_flags() {
        assert_eq!(
            mode(&["daimonos", "--mcp", "agent", "do work", "--dry-run"]),
            RuntimeMode::Agent
        );
    }

    #[test]
    fn conflicting_legacy_mcp_transports_are_rejected() {
        assert!(
            Cli::try_parse_from(["daimonos", "--mcp", "--mcp-socket", "/tmp/mcp.sock",]).is_err()
        );
    }

    #[test]
    fn agent_output_defaults_to_text_and_accepts_json() {
        let agent_output = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Agent(args)) => args.output,
            _ => panic!("expected the agent command"),
        };
        assert_eq!(
            agent_output(&["daimonos", "agent", "task"]),
            AgentOutputFormat::Text
        );
        assert_eq!(
            agent_output(&["daimonos", "agent", "--output", "json", "task"]),
            AgentOutputFormat::Json
        );
        assert!(Cli::try_parse_from(["daimonos", "agent", "--output", "yaml", "task"]).is_err());
    }

    #[test]
    fn agent_output_json_conflicts_with_interactive_and_dry_run() {
        for flag in ["--interactive", "--dry-run"] {
            assert!(
                Cli::try_parse_from(["daimonos", "agent", flag, "--output", "json", "task"])
                    .is_err(),
                "{flag} must conflict with --output json"
            );
        }
    }

    #[test]
    fn agent_session_id_is_separate_from_the_top_level_session_id() {
        let parse = |argv: &[&str]| {
            let cli = Cli::try_parse_from(argv).unwrap();
            let agent = match cli.command {
                Some(Command::Agent(args)) => args.oneshot_session_id,
                _ => panic!("expected the agent command"),
            };
            (cli.session_id, agent)
        };
        assert_eq!(
            parse(&[
                "daimonos",
                "--session-id",
                "TOP",
                "agent",
                "--session-id",
                "SUB",
                "t"
            ]),
            (Some("TOP".to_string()), Some("SUB".to_string()))
        );
        assert_eq!(
            parse(&["daimonos", "agent", "--session-id", "SUB", "t"]),
            (None, Some("SUB".to_string()))
        );
        assert_eq!(
            parse(&["daimonos", "--session-id", "TOP", "agent", "t"]),
            (Some("TOP".to_string()), None)
        );
        for flag in ["--interactive", "--dry-run"] {
            assert!(
                Cli::try_parse_from(["daimonos", "agent", flag, "--session-id", "SUB", "t"])
                    .is_err(),
                "{flag} must conflict with agent --session-id"
            );
        }
    }

    #[test]
    fn agent_resume_conflicts_with_session_id_interactive_and_dry_run() {
        let cli = Cli::try_parse_from(["daimonos", "agent", "--resume", "run-1", "more"]).unwrap();
        match cli.command {
            Some(Command::Agent(args)) => assert_eq!(args.resume.as_deref(), Some("run-1")),
            _ => panic!("expected the agent command"),
        }
        for extra in [
            &["--session-id", "run-2"][..],
            &["--interactive"][..],
            &["--dry-run"][..],
        ] {
            let mut argv = vec!["daimonos", "agent", "--resume", "run-1"];
            argv.extend_from_slice(extra);
            argv.push("more");
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{extra:?} must conflict with --resume"
            );
        }
    }

    /// The one-shot session options and `--output json` (ADR-036) are refused
    /// with the reviewer-runtime options that they were not built with.
    #[test]
    fn agent_session_and_json_options_conflict_with_unbuilt_combinations() {
        let refused = |extra: &[&str]| {
            let mut argv = vec!["daimonos", "agent"];
            argv.extend_from_slice(extra);
            argv.push("task");
            Cli::try_parse_from(argv).is_err()
        };
        for session in [["--session-id", "s1"], ["--resume", "s1"]] {
            for other in [
                ["--final-output", "f.json"],
                ["--model-profile", "p.json"],
                ["--worker-config", "w.json"],
            ] {
                assert!(
                    refused(&[session[0], session[1], other[0], other[1]]),
                    "{session:?} with {other:?}"
                );
            }
        }
        assert!(refused(&["--output", "json", "--final-output", "f.json"]));
        assert!(refused(&[
            "--output",
            "json",
            "--capability-probe-payload",
            "p.json"
        ]));
        // Combinations that stay available.
        assert!(!refused(&["--final-output", "f.json"]));
        assert!(!refused(&["--output", "json", "--model-profile", "p.json"]));
        assert!(!refused(&["--output", "json", "--worker-config", "w.json"]));
        assert!(!refused(&["--session-id", "s1", "--output", "json"]));
    }
}
