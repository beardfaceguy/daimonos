use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct AgentArgs {
    /// Task description for the agent
    pub task: String,
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
pub struct McpArgs {
    /// Serve MCP over this Unix socket instead of stdio.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the agent on a one-shot task and exit.
    Agent(AgentArgs),
    /// Start an interactive chat REPL over a stateful agent session.
    Chat(ChatArgs),
    /// Run a native Agent Client Protocol engine over stdio.
    Acp(AcpArgs),
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
        assert_eq!(mode(&["daimonos", "--stats"]), RuntimeMode::Stats);
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
