use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

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
    /// Run the compact opcode protocol daemon over a Unix socket.
    Daemon,
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
    Daemon,
    Stats,
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
            Self::Daemon => "socket",
            Self::Stats => "stats",
        }
    }

    pub fn is_mcp_stdio(&self) -> bool {
        matches!(self, Self::McpStdio)
    }
}

#[derive(Debug, Parser)]
#[command(name = "daimonos", about = "Daimonos — agent-optimized OS layer")]
pub struct Cli {
    /// Unix socket path used by `daemon` and the legacy default mode.
    #[arg(short, long, default_value = "/tmp/daimonos.sock")]
    pub socket: PathBuf,

    /// Workspace root directory.
    #[arg(short, long, default_value = ".")]
    pub workspace: PathBuf,

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

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Resolve old flags and normalized subcommands to one runtime identity.
    /// Explicit subcommands retain the historical precedence over legacy flags.
    pub fn runtime_mode(&self) -> RuntimeMode {
        match &self.command {
            Some(Command::Agent(_)) => RuntimeMode::Agent,
            Some(Command::Chat(_)) => RuntimeMode::Chat,
            Some(Command::Acp(_)) => RuntimeMode::Acp,
            Some(Command::SessionDaemon(_)) => RuntimeMode::SessionDaemon,
            Some(Command::Session(_)) => RuntimeMode::Session,
            Some(Command::Mcp(args)) => args
                .socket
                .clone()
                .map(RuntimeMode::McpSocket)
                .unwrap_or(RuntimeMode::McpStdio),
            Some(Command::Daemon) => RuntimeMode::Daemon,
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
}
