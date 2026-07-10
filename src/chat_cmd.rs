use std::path::{Path, PathBuf};
use std::sync::Arc;

use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};

use crate::agent::{AgentConfig, AgentSession, TokenLogConfig};
use crate::agent_cmd::default_system_prompt;
use crate::config::Config;
use crate::providers::{CompleteOpts, ContentBlock, LlmProvider, Message, Role, StreamEvent, ToolSchema};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::session_store::{SessionStore, SessionSummary};
use crate::tool_facade;

const HELP_TEXT: &str = "\
Commands:
  /exit, exit, quit   quit the chat session
  /clear              reset conversation history (cumulative usage is kept)
  /usage              show cumulative token usage for this session
  /help               show this message
Ctrl-C aborts the in-flight turn without quitting; Ctrl-D quits.";

const CTRL_C_IDLE_HINT: &str = "(use /exit, exit, or Ctrl-D to quit)";

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
///
/// Bare `exit`/`quit` (no leading slash) are also accepted as `Exit`, since
/// that's the first thing most people type to leave a REPL.
pub fn parse_line(line: &str) -> ChatCommand {
    let trimmed = line.trim();
    match trimmed {
        "/exit" | "/quit" => ChatCommand::Exit,
        "/clear" => ChatCommand::Clear,
        "/help" => ChatCommand::Help,
        "/usage" => ChatCommand::Usage,
        other => match other.to_ascii_lowercase().as_str() {
            "exit" | "quit" => ChatCommand::Exit,
            _ => ChatCommand::Prompt(other.to_string()),
        },
    }
}

/// Build the [`AgentConfig`] for a chat session, mirroring
/// `agent_cmd::run_agent`'s construction (system prompt, active tool schemas,
/// model, and safety hook), plus a stream hook that prints assistant text
/// deltas live as they arrive (vikunja #957) — thinking/tool-call deltas are
/// captured by the provider but not rendered, matching prior non-streaming
/// behavior where thinking was never shown in the REPL.
pub fn build_agent_config(
    workspace: &Path,
    model: String,
    safety: Option<SafetyPolicy>,
    token_log: Option<std::path::PathBuf>,
) -> AgentConfig {
    let tools: Vec<ToolSchema> = tool_facade::active_schemas(workspace)
        .into_iter()
        .map(|s| ToolSchema { name: s.name, description: s.description, input_schema: s.input_schema })
        .collect();
    AgentConfig {
        system: Some(default_system_prompt()),
        tools,
        opts: CompleteOpts { model, ..CompleteOpts::default() },
        before_tool_call: safety.map(|p| p.into_before_hook()),
        on_stream_event: Some(Box::new(|ev| {
            if let StreamEvent::TextDelta(text) = ev {
                use std::io::Write;
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
        })),
        token_log: token_log.map(|path| TokenLogConfig { path, label: "chat".to_string() }),
        ..AgentConfig::default()
    }
}

/// Build the tool-facing `Session` for a chat REPL from the already-loaded
/// CLI/project config (vikunja #958), instead of a hardcoded
/// `Config::default()` that would silently ignore user-configured settings
/// (verbosity, `process.extra_path`, MCP settings, etc.).
fn build_tool_session(workspace: &Path, cfg: Arc<Config>) -> Session {
    Session::new(workspace.to_path_buf(), cfg)
}

/// Load a saved session's history for `--resume`, erroring (rather than
/// silently starting fresh) if the id isn't found — mirrors the ACP
/// `session/load` "no session found" decision (vikunja #961).
fn load_resume(store: Option<&SessionStore>, id: &str) -> anyhow::Result<Vec<Message>> {
    match store.and_then(|s| s.load(id)) {
        Some(record) => Ok(record.messages),
        None => anyhow::bail!("no saved chat session with id '{id}' (try `daimonos chat --list`)"),
    }
}

/// Render a resumed conversation for the terminal so the user sees prior
/// context on `--resume`. User messages are prefixed with `> `, assistant
/// text is shown plainly, tool calls are summarized; tool results and
/// thinking are omitted to keep the recap readable.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Text(text) => match message.role {
                    Role::User => {
                        out.push_str("> ");
                        out.push_str(text);
                        out.push('\n');
                    }
                    Role::Assistant => {
                        out.push_str(text);
                        out.push('\n');
                    }
                },
                ContentBlock::ToolCall { name, .. } => out.push_str(&format!("[tool: {name}]\n")),
                ContentBlock::Thinking(_) | ContentBlock::ToolResult { .. } => {}
            }
        }
    }
    out
}

/// Format the `daimonos chat --list` output: one line per saved session with
/// its id, model, message count, and first-user-line label.
pub fn format_session_list(sessions: &[SessionSummary]) -> String {
    if sessions.is_empty() {
        return "no saved chat sessions".to_string();
    }
    sessions
        .iter()
        .map(|s| {
            let label = s.first_user_line.as_deref().unwrap_or("(empty)");
            format!("{}  [{}]  {} msgs  {}", s.id, s.model, s.message_count, label)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run the interactive `daimonos chat` REPL to completion (`/exit` or Ctrl-D).
#[allow(clippy::too_many_arguments)]
pub async fn run_chat(
    provider: Box<dyn LlmProvider>,
    workspace: &Path,
    cfg: Arc<Config>,
    model: String,
    safety: Option<SafetyPolicy>,
    token_log: Option<std::path::PathBuf>,
    sessions_dir: Option<PathBuf>,
    resume: Option<String>,
) -> anyhow::Result<()> {
    let config = build_agent_config(workspace, model, safety, token_log);
    let tool_session = build_tool_session(workspace, cfg);
    let mut session = AgentSession::new(provider, tool_session, config);

    // Persist to disk so the conversation can be resumed later (vikunja #963).
    // `None` disables persistence (tests). A resumed session keeps its id;
    // a fresh one mints a uuid.
    let store = sessions_dir.map(SessionStore::new);
    let session_id = resume.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // On --resume, restore the prior history and echo the transcript so the
    // user sees where they left off before the next prompt.
    if let Some(id) = &resume {
        let history = load_resume(store.as_ref(), id)?;
        print!("{}", render_transcript(&history));
        session.set_history(history);
        println!("[resumed session {id}]");
    }

    let mut line_editor = Reedline::create();
    // Distinct "*D*" left segment so the REPL prompt is never mistaken for a
    // regular shell prompt (which the default reedline cwd-based prompt resembles).
    let prompt = DefaultPrompt::new(DefaultPromptSegment::Basic("*D*".to_string()), DefaultPromptSegment::Empty);

    println!("daimonos chat [{session_id}] — type /help for commands, Ctrl-D to quit.");

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buffer)) => match parse_line(&buffer) {
                ChatCommand::Exit => break,
                ChatCommand::Clear => {
                    session.clear();
                    if let Some(store) = &store {
                        store.save(&session_id, session.model(), session.history());
                    }
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
                    let completed = tokio::select! {
                        turn = session.prompt(text) => {
                            if let Some(err) = &turn.error_message {
                                eprintln!("[error] {err}");
                            }
                            // Text was already printed live via on_stream_event;
                            // just close out the line it was streamed on.
                            if !turn.text.is_empty() {
                                println!();
                            }
                            true
                        }
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!("\n[turn aborted]");
                            false
                        }
                    };
                    // Persist only a completed turn — an aborted one leaves
                    // history unchanged (prompt is cancel-safe), so there's
                    // nothing new to save.
                    if completed {
                        if let Some(store) = &store {
                            store.save(&session_id, session.model(), session.history());
                        }
                    }
                }
            },
            Ok(Signal::CtrlC) => {
                eprintln!("{CTRL_C_IDLE_HINT}");
                continue;
            }
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
    fn parses_slash_quit() {
        assert_eq!(parse_line("/quit"), ChatCommand::Exit);
    }

    #[test]
    fn parses_bare_exit() {
        assert_eq!(parse_line("exit"), ChatCommand::Exit);
    }

    #[test]
    fn parses_bare_quit() {
        assert_eq!(parse_line("quit"), ChatCommand::Exit);
    }

    #[test]
    fn parses_bare_exit_case_insensitively() {
        assert_eq!(parse_line("EXIT"), ChatCommand::Exit);
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
        let config = build_agent_config(dir.path(), "claude-haiku-4-5".to_string(), None, None);
        assert_eq!(config.opts.model, "claude-haiku-4-5");
    }

    #[test]
    fn config_has_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None, None);
        assert!(config.system.is_some());
    }

    #[test]
    fn config_includes_tool_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None, None);
        assert!(!config.tools.is_empty(), "chat session should expose tools like the agent subcommand");
    }

    #[test]
    fn config_wires_stream_hook_for_live_text_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None, None);
        assert!(config.on_stream_event.is_some(), "chat session should stream text deltas live");
    }

    #[test]
    fn config_has_no_token_log_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None, None);
        assert!(config.token_log.is_none());
    }

    #[test]
    fn config_wires_token_log_with_chat_label() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let config = build_agent_config(dir.path(), "m".to_string(), None, Some(log_path.clone()));
        let log_cfg = config.token_log.expect("--debug-tokens should wire a TokenLogConfig");
        assert_eq!(log_cfg.path, log_path);
        assert_eq!(log_cfg.label, "chat");
    }

    #[test]
    fn config_has_no_before_hook_without_safety_policy() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(dir.path(), "m".to_string(), None, None);
        assert!(config.before_tool_call.is_none());
    }

    #[test]
    fn config_wires_safety_policy_into_before_hook() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SafetyPolicy { denied_commands: vec!["exec".into()], ..SafetyPolicy::default() };
        let config = build_agent_config(dir.path(), "m".to_string(), Some(policy), None);
        assert!(config.before_tool_call.is_some());
    }

    // --- build_tool_session (vikunja #958) ---

    #[test]
    fn tool_session_uses_provided_cfg_not_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.mcp.default_verbosity = crate::verbosity::Verbosity::Terse;
        assert_ne!(
            cfg.mcp.default_verbosity,
            Config::default().mcp.default_verbosity,
            "test setup must pick a non-default verbosity to prove threading"
        );

        let session = build_tool_session(dir.path(), Arc::new(cfg));
        assert_eq!(session.verbosity, crate::verbosity::Verbosity::Terse);
    }

    // --- session persistence / resume (vikunja #963) ---

    #[test]
    fn load_resume_returns_saved_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("sess-1", "m", &[Message::user("prior question"), Message::assistant("prior answer")]);

        let history = load_resume(Some(&store), "sess-1").expect("saved session should resume");
        assert_eq!(history.len(), 2);
        assert!(matches!(&history[0].content[0], ContentBlock::Text(t) if t == "prior question"));
    }

    #[test]
    fn load_resume_unknown_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        assert!(
            load_resume(Some(&store), "nope").is_err(),
            "resuming an unknown id must error, not start a silent fresh session"
        );
    }

    #[test]
    fn load_resume_without_store_errors() {
        assert!(load_resume(None, "any").is_err(), "resume with persistence disabled must error");
    }

    #[test]
    fn render_transcript_shows_user_and_assistant_skips_noise() {
        let messages = vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking("(hidden)".into()),
                    ContentBlock::Text("hi there".into()),
                    ContentBlock::ToolCall {
                        id: "t1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            // Tool result comes back as a User-role message; must be skipped.
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file contents".into(),
                    is_error: false,
                }],
            },
        ];
        let out = render_transcript(&messages);
        assert!(out.contains("> hello"), "user message should be prefixed: {out:?}");
        assert!(out.contains("hi there"), "assistant text should show: {out:?}");
        assert!(out.contains("[tool: read_file]"), "tool call should be summarized: {out:?}");
        assert!(!out.contains("hidden"), "thinking must be omitted: {out:?}");
        assert!(!out.contains("file contents"), "tool result must be omitted: {out:?}");
    }

    #[test]
    fn format_session_list_empty_and_populated() {
        assert_eq!(format_session_list(&[]), "no saved chat sessions");

        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("sess-1", "model-x", &[Message::user("do the thing")]);
        let listed = format_session_list(&store.list());
        assert!(listed.contains("sess-1"), "list should show the id: {listed:?}");
        assert!(listed.contains("model-x"), "list should show the model: {listed:?}");
        assert!(listed.contains("do the thing"), "list should show the label: {listed:?}");
    }
}
