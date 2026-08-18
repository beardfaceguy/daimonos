use std::path::{Path, PathBuf};
use std::sync::Arc;

use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
use tracing::Instrument;

use crate::agent::{AgentConfig, AgentSession, TokenLogConfig};
use crate::compaction::CompactionPolicy;
use crate::config::Config;
use crate::observability::{PromptMetadata, PromptSpan};
use crate::providers::{
    CompleteOpts, ContentBlock, LlmProvider, Message, Role, StreamEvent, ToolSchema,
};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::session_store::{PersistedSession, SessionStore, SessionSummary};
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
#[cfg(test)]
pub fn build_agent_config(
    workspace: &Path,
    model: String,
    safety: Option<SafetyPolicy>,
    token_log: Option<std::path::PathBuf>,
    compaction: Option<CompactionPolicy>,
    system_prompt: String,
) -> AgentConfig {
    build_agent_config_with_descriptions(
        workspace,
        model,
        safety,
        token_log,
        compaction,
        system_prompt,
        &crate::tool_descriptions::ToolDescriptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_agent_config_with_descriptions(
    workspace: &Path,
    model: String,
    safety: Option<SafetyPolicy>,
    token_log: Option<std::path::PathBuf>,
    compaction: Option<CompactionPolicy>,
    system_prompt: String,
    descriptions: &crate::tool_descriptions::ToolDescriptions,
) -> AgentConfig {
    let tools: Vec<ToolSchema> = tool_facade::active_schemas(workspace, descriptions)
        .into_iter()
        .map(|s| ToolSchema {
            name: s.name,
            description: s.description,
            input_schema: s.input_schema,
        })
        .collect();
    AgentConfig {
        system: Some(system_prompt),
        tools,
        opts: CompleteOpts {
            model,
            ..CompleteOpts::default()
        },
        before_tool_call: safety.map(|p| p.into_before_hook()),
        on_stream_event: Some(Box::new(|ev| {
            if let StreamEvent::TextDelta(text) = ev {
                use std::io::Write;
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
        })),
        token_log: token_log.map(|path| TokenLogConfig {
            path,
            label: "chat".to_string(),
        }),
        compaction,
        // Informational REPL notice (ADR-002 Q6) — compaction never rewrites
        // what's already on screen, only what gets sent to the model.
        on_compaction: Some(Box::new(|event| {
            println!(
                "[context compacted — summarized {} older turn(s)]",
                event.evicted_turns
            );
        })),
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

/// Load a saved session for `--resume`, erroring (rather than silently
/// starting fresh) if the id isn't found — mirrors the ACP `session/load`
/// "no session found" decision (vikunja #961).
fn load_resume(store: Option<&SessionStore>, id: &str) -> anyhow::Result<PersistedSession> {
    match store.and_then(|s| s.load(id)) {
        Some(record) => Ok(record),
        None => anyhow::bail!("no saved chat session with id '{id}' (try `daimonos chat --list`)"),
    }
}

/// Pick the model for a (possibly resumed) chat session: an explicit
/// `--model` always wins; otherwise a resumed session prefers the model it
/// was saved on; otherwise the launch default (flag-or-env) stands. (The
/// terminal REPL has no live model picker — that's the ACP/Zed frontend — so
/// this is the only place a resumed session's model is honored.)
fn resolve_model(launch_model: &str, model_explicit: bool, resumed_model: Option<&str>) -> String {
    match resumed_model {
        Some(saved) if !model_explicit => saved.to_string(),
        _ => launch_model.to_string(),
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
                ContentBlock::Image {
                    media_type, uri, ..
                } => out.push_str(&format!(
                    "[image: {media_type}{}]\n",
                    uri.as_deref()
                        .map(|uri| format!(" {uri}"))
                        .unwrap_or_default()
                )),
                ContentBlock::ToolCall { name, .. } => out.push_str(&format!("[tool: {name}]\n")),
                ContentBlock::Thinking(_)
                | ContentBlock::ProviderState { .. }
                | ContentBlock::ToolResult { .. } => {}
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
            format!(
                "{}  [{}]  {} msgs  {}",
                s.id, s.model, s.message_count, label
            )
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
    model_explicit: bool,
    safety: Option<SafetyPolicy>,
    token_log: Option<std::path::PathBuf>,
    sessions_dir: Option<PathBuf>,
    resume: Option<String>,
    compaction: Option<CompactionPolicy>,
) -> anyhow::Result<()> {
    let system_prompt = crate::prompts::agent_system(&cfg).await;
    let mut config = build_agent_config_with_descriptions(
        workspace,
        model.clone(),
        safety,
        token_log,
        compaction,
        system_prompt,
        &cfg.prompts.resolved_tool_descriptions,
    );
    // Outbound MCP servers (#1289): the chat REPL reads the Claude-style
    // file named by `[agent.mcp]`.
    let native_names: std::collections::HashSet<String> =
        config.tools.iter().map(|tool| tool.name.clone()).collect();
    let agent_mcp = crate::agent_mcp::connect(&cfg, &native_names, None).await;
    if let Some(mcp) = &agent_mcp {
        config.tools.extend(mcp.tools());
        config.remote_tool_dispatch = Some(mcp.dispatch_hook());
    }
    let tool_session = build_tool_session(workspace, cfg);
    let mut session = AgentSession::new(provider, tool_session, config);

    // Persist to disk so the conversation can be resumed later (vikunja #963).
    // `None` disables persistence (tests). A resumed session keeps its id;
    // a fresh one mints a uuid.
    let store = sessions_dir.map(SessionStore::new);
    let session_id = resume
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // On --resume, restore the prior history and echo the transcript so the
    // user sees where they left off before the next prompt.
    if let Some(id) = &resume {
        let record = load_resume(store.as_ref(), id)?;
        print!("{}", render_transcript(&record.messages));
        // Prefer the model the session was saved on, unless --model was passed.
        let resolved = resolve_model(&model, model_explicit, Some(&record.model));
        session.set_history(record.messages);
        session.set_model(resolved.as_str());
        println!("[resumed session {id} (model: {resolved})]");
    }

    let mut line_editor = Reedline::create();
    // Distinct "*D*" left segment so the REPL prompt is never mistaken for a
    // regular shell prompt (which the default reedline cwd-based prompt resembles).
    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("*D*".to_string()),
        DefaultPromptSegment::Empty,
    );

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
                    let prompt_span = PromptSpan::new(PromptMetadata {
                        mode: "chat",
                        session_id: Some(&session_id),
                        model: session.model(),
                        workspace,
                        turn_index: session.user_turn_count(),
                        tools_exposed: session.tool_count(),
                    });
                    let prompt = session.prompt(text).instrument(prompt_span.span().clone());
                    let outcome = tokio::select! {
                        turn = prompt => Some(turn),
                        _ = tokio::signal::ctrl_c() => None,
                    };
                    let completed = match outcome {
                        Some(turn) => {
                            if let Some(err) = &turn.error_message {
                                eprintln!("[error] {err}");
                            }
                            // Text was already printed live via on_stream_event;
                            // just close out the line it was streamed on.
                            if !turn.text.is_empty() {
                                println!();
                            }
                            let error_type = match turn.stop_reason {
                                crate::providers::StopReason::Error => Some("provider_error"),
                                crate::providers::StopReason::Refusal => Some("refusal"),
                                _ => None,
                            };
                            // An `Aborted` turn was terminated by the policy
                            // hook, not the client (ADR-006 D5).
                            if matches!(turn.stop_reason, crate::providers::StopReason::Aborted) {
                                prompt_span.record_cancel_reason("policy");
                            }
                            prompt_span.finish(turn.stop_reason.as_str(), error_type);
                            true
                        }
                        None => {
                            eprintln!("\n[turn aborted]");
                            prompt_span.record_cancel_reason("client");
                            prompt_span.finish("cancelled", Some("client_cancelled"));
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

    if let Some(mcp) = agent_mcp {
        mcp.shutdown().await;
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
        assert_eq!(
            parse_line("hello there"),
            ChatCommand::Prompt("hello there".to_string())
        );
    }

    #[test]
    fn unrecognized_slash_command_is_treated_as_a_prompt() {
        assert_eq!(
            parse_line("/bogus"),
            ChatCommand::Prompt("/bogus".to_string())
        );
    }

    #[test]
    fn blank_line_is_an_empty_prompt() {
        assert_eq!(parse_line("   "), ChatCommand::Prompt(String::new()));
    }

    // --- build_agent_config ---

    #[test]
    fn config_uses_given_model() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(
            dir.path(),
            "claude-haiku-4-5".to_string(),
            None,
            None,
            None,
            "sys".to_string(),
        );
        assert_eq!(config.opts.model, "claude-haiku-4-5");
    }

    #[test]
    fn config_has_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            None,
            None,
            None,
            "sys".to_string(),
        );
        assert!(config.system.is_some());
    }

    #[test]
    fn config_includes_tool_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            None,
            None,
            None,
            "sys".to_string(),
        );
        assert!(
            !config.tools.is_empty(),
            "chat session should expose tools like the agent subcommand"
        );
    }

    #[test]
    fn config_wires_stream_hook_for_live_text_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            None,
            None,
            None,
            "sys".to_string(),
        );
        assert!(
            config.on_stream_event.is_some(),
            "chat session should stream text deltas live"
        );
    }

    #[test]
    fn config_has_no_token_log_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            None,
            None,
            None,
            "sys".to_string(),
        );
        assert!(config.token_log.is_none());
    }

    #[test]
    fn config_wires_token_log_with_chat_label() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            None,
            Some(log_path.clone()),
            None,
            "sys".to_string(),
        );
        let log_cfg = config
            .token_log
            .expect("--debug-tokens should wire a TokenLogConfig");
        assert_eq!(log_cfg.path, log_path);
        assert_eq!(log_cfg.label, "chat");
    }

    #[test]
    fn config_has_no_before_hook_without_safety_policy() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            None,
            None,
            None,
            "sys".to_string(),
        );
        assert!(config.before_tool_call.is_none());
    }

    #[test]
    fn config_wires_safety_policy_into_before_hook() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SafetyPolicy {
            denied_commands: vec!["exec".into()],
            ..SafetyPolicy::default()
        };
        let config = build_agent_config(
            dir.path(),
            "m".to_string(),
            Some(policy),
            None,
            None,
            "sys".to_string(),
        );
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
    fn load_resume_returns_saved_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save(
            "sess-1",
            "saved-model",
            &[
                Message::user("prior question"),
                Message::assistant("prior answer"),
            ],
        );

        let record = load_resume(Some(&store), "sess-1").expect("saved session should resume");
        assert_eq!(record.messages.len(), 2);
        assert_eq!(record.model, "saved-model");
        assert!(
            matches!(&record.messages[0].content[0], ContentBlock::Text(t) if t == "prior question")
        );
    }

    #[test]
    fn resolve_model_prefers_saved_unless_flag_explicit() {
        // Resuming, no explicit --model: use the model the session was saved on.
        assert_eq!(
            resolve_model("launch-default", false, Some("saved-model")),
            "saved-model"
        );
        // Resuming, explicit --model: the flag wins over the saved model.
        assert_eq!(
            resolve_model("flag-model", true, Some("saved-model")),
            "flag-model"
        );
        // Fresh session (no resume): the launch model stands, explicit or not.
        assert_eq!(
            resolve_model("launch-default", false, None),
            "launch-default"
        );
        assert_eq!(resolve_model("flag-model", true, None), "flag-model");
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
        assert!(
            load_resume(None, "any").is_err(),
            "resume with persistence disabled must error"
        );
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
        assert!(
            out.contains("> hello"),
            "user message should be prefixed: {out:?}"
        );
        assert!(
            out.contains("hi there"),
            "assistant text should show: {out:?}"
        );
        assert!(
            out.contains("[tool: read_file]"),
            "tool call should be summarized: {out:?}"
        );
        assert!(!out.contains("hidden"), "thinking must be omitted: {out:?}");
        assert!(
            !out.contains("file contents"),
            "tool result must be omitted: {out:?}"
        );
    }

    #[test]
    fn format_session_list_empty_and_populated() {
        assert_eq!(format_session_list(&[]), "no saved chat sessions");

        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("sess-1", "model-x", &[Message::user("do the thing")]);
        let listed = format_session_list(&store.list());
        assert!(
            listed.contains("sess-1"),
            "list should show the id: {listed:?}"
        );
        assert!(
            listed.contains("model-x"),
            "list should show the model: {listed:?}"
        );
        assert!(
            listed.contains("do the thing"),
            "list should show the label: {listed:?}"
        );
    }
}
