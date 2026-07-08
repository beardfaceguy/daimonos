use std::path::Path;
use std::sync::Arc;

use reedline::{DefaultPrompt, Reedline, Signal};

use crate::agent::{AgentConfig, AgentSession};
use crate::agent_cmd::default_system_prompt;
use crate::config::Config;
use crate::providers::{CompleteOpts, LlmProvider, ToolSchema};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::tool_facade;

const HELP_TEXT: &str = "\
Commands:
  /exit    quit the chat session
  /clear   reset conversation history (cumulative usage is kept)
  /usage   show cumulative token usage for this session
  /help    show this message
Ctrl-C aborts the in-flight turn without quitting; Ctrl-D quits.";

/// One parsed line of `daimonos chat` REPL input.
#[derive(Debug, PartialEq, Eq)]
pub enum ChatCommand {
    Exit,
    Clear,
    Help,
    Usage,
    Prompt(String),
}

/// Parse one line of REPL input. Everything that isn't a recognized slash
/// command becomes a `Prompt` (trimmed; may be empty for a blank line).
pub fn parse_line(line: &str) -> ChatCommand {
    match line.trim() {
        "/exit" => ChatCommand::Exit,
        "/clear" => ChatCommand::Clear,
        "/help" => ChatCommand::Help,
        "/usage" => ChatCommand::Usage,
        other => ChatCommand::Prompt(other.to_string()),
    }
}

/// Build the [`AgentConfig`] for a chat session, mirroring
/// `agent_cmd::run_agent`'s construction (system prompt, active tool schemas,
/// model, and safety hook).
pub fn build_agent_config(workspace: &Path, model: String, safety: Option<SafetyPolicy>) -> AgentConfig {
    let tools: Vec<ToolSchema> = tool_facade::active_schemas(workspace)
        .into_iter()
        .map(|s| ToolSchema { name: s.name, description: s.description, input_schema: s.input_schema })
        .collect();
    AgentConfig {
        system: Some(default_system_prompt()),
        tools,
        opts: CompleteOpts { model, ..CompleteOpts::default() },
        before_tool_call: safety.map(|p| p.into_before_hook()),
        ..AgentConfig::default()
    }
}

/// Run the interactive `daimonos chat` REPL to completion (`/exit` or Ctrl-D).
pub async fn run_chat(
    provider: Box<dyn LlmProvider>,
    workspace: &Path,
    model: String,
    safety: Option<SafetyPolicy>,
) -> anyhow::Result<()> {
    let config = build_agent_config(workspace, model, safety);
    let tool_session = Session::new(workspace.to_path_buf(), Arc::new(Config::default()));
    let mut session = AgentSession::new(provider, tool_session, config);

    let mut line_editor = Reedline::create();
    let prompt = DefaultPrompt::default();

    println!("daimonos chat — type /help for commands, Ctrl-D to quit.");

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buffer)) => match parse_line(&buffer) {
                ChatCommand::Exit => break,
                ChatCommand::Clear => {
                    session.clear();
                    println!("[history cleared]");
                }
                ChatCommand::Help => println!("{HELP_TEXT}"),
                ChatCommand::Usage => {
                    let u = session.total_usage();
                    println!(
                        "input={} output={} cache_read={} cache_write={} cost=${:.4}",
                        u.input, u.output, u.cache_read, u.cache_write, u.cost.total_usd
                    );
                }
                ChatCommand::Prompt(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    tokio::select! {
                        turn = session.prompt(text) => {
                            if let Some(err) = &turn.error_message {
                                eprintln!("[error] {err}");
                            }
                            if !turn.text.is_empty() {
                                println!("{}", turn.text);
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!("\n[turn aborted]");
                        }
                    }
                }
            },
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_line ---

    #[test]
    fn parses_exit() {
        assert_eq!(parse_line("/exit"), ChatCommand::Exit);
    }

    #[test]
    fn parses_clear() {
        assert_eq!(parse_line("/clear"), ChatCommand::Clear);
    }

    #[test]
    fn parses_help() {
        assert_eq!(parse_line("/help"), ChatCommand::Help);
    }

    #[test]
    fn parses_usage() {
        assert_eq!(parse_line("/usage"), ChatCommand::Usage);
    }

    #[test]
    fn trims_whitespace_before_matching_slash_commands() {
        assert_eq!(parse_line("  /exit  "), ChatCommand::Exit);
    }

    #[test]
    fn plain_text_is_a_prompt() {
        assert_eq!(parse_line("hello there"), ChatCommand::Prompt("hello there".to_string()));
    }

    #[test]
    fn unrecognized_slash_command_is_treated_as_a_prompt() {
        assert_eq!(parse_line("/bogus"), ChatCommand::Prompt("/bogus".to_string()));
    }

    #[test]
    fn blank_line_is_an_empty_prompt() {
        assert_eq!(parse_line("   "), ChatCommand::Prompt(String::new()));
    }

    // --- build_agent_config ---

    #[test]
    fn config_uses_given_model() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "claude-haiku-4-5".to_string(), None);
        assert_eq!(config.opts.model, "claude-haiku-4-5");
    }

    #[test]
    fn config_has_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None);
        assert!(config.system.is_some());
    }

    #[test]
    fn config_includes_tool_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None);
        assert!(!config.tools.is_empty(), "chat session should expose tools like the agent subcommand");
    }

    #[test]
    fn config_has_no_before_hook_without_safety_policy() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None);
        assert!(config.before_tool_call.is_none());
    }

    #[test]
    fn config_wires_safety_policy_into_before_hook() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SafetyPolicy { denied_commands: vec!["exec".into()], ..SafetyPolicy::default() };
        let config = build_agent_config(dir.path(), "m".to_string(), Some(policy));
        assert!(config.before_tool_call.is_some());
    }
}
