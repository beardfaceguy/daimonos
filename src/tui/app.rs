//! Local full-screen frontend for a stateful [`AgentSession`] (Vikunja #1091).
//!
//! The event hooks project provider and tool activity into the same canonical
//! [`SessionEvent`] stream used by ACP clients. Rendering reads only
//! [`ViewState`]; it never reaches into the provider or tool loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use tokio::sync::oneshot;

use crate::agent::{
    AfterHookResult, AgentConfig, AgentSession, BeforeHookResult, TokenLogConfig, ToolCallInfo,
    TurnResult,
};
use crate::analytics::AnalyticsStore;
use crate::compaction::CompactionPolicy;
use crate::config::Config;
use crate::providers::{
    CompleteOpts, LlmProvider, StopReason, StreamEvent, ThinkingLevel, ToolSchema,
};
use crate::safety::{Gate, SafetyPolicy};
use crate::session::Session;
use crate::session_core::{SessionEventHandler, SessionEventRouter};
use crate::session_protocol::{
    ApprovalDecision, ApprovalRequest, AssistantOutcome, ContextUsage, RuntimeChoice,
    RuntimeOption, RuntimeValue, SessionEvent, ToolCallStateStatus, TurnStatus,
};
use crate::tool_facade;

use super::commands::{approval_from_key, parse_command, UiCommand, HELP_TEXT};
use super::input::{ComposerHistory, TranscriptScroll};
use super::state::ViewState;
use super::terminal::{install_panic_hook, TerminalGuard};

/// Runtime inputs already resolved by `agent_runtime`.
pub struct TuiOptions {
    pub initial_prompt: Option<String>,
    pub no_color: bool,
    pub model: String,
    pub models: Vec<String>,
    pub safety: SafetyPolicy,
    pub token_log: Option<PathBuf>,
    pub compaction: Option<CompactionPolicy>,
    pub analytics: Option<Arc<AnalyticsStore>>,
    pub thinking: ThinkingLevel,
}

struct PendingApproval {
    sender: oneshot::Sender<ApprovalDecision>,
}

type SharedView = Arc<StdMutex<ViewState>>;
type SharedApproval = Arc<StdMutex<Option<PendingApproval>>>;

/// Run the opt-in full-screen TUI until quit or stop-session.
pub async fn run(
    provider: Box<dyn LlmProvider>,
    workspace: &Path,
    cfg: Arc<Config>,
    options: TuiOptions,
) -> anyhow::Result<()> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let history_entries = cfg.tui.history_entries;
    let scrollback_entries = cfg.tui.scrollback_entries;
    let view = Arc::new(StdMutex::new(ViewState::with_scrollback_limit(
        session_id.clone(),
        scrollback_entries,
    )));
    let handler_view = Arc::clone(&view);
    let handler: SessionEventHandler = Arc::new(move |seq, event| {
        let mut view = handler_view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = view.apply_event(seq, event);
    });
    let events = Arc::new(SessionEventRouter::new(Some(handler)));
    let pending_approval: SharedApproval = Arc::new(StdMutex::new(None));
    let streamed_text = Arc::new(AtomicBool::new(false));

    let runtime_options = model_runtime_options(&options.models, &options.model);
    let _ = events.emit(SessionEvent::RuntimeOptionsChanged {
        options: runtime_options,
    });

    let tools = active_tools(workspace, &cfg);
    let safety = Arc::new(options.safety);
    let before_tool_call = build_before_tool_call_hook(
        Arc::clone(&events),
        Arc::clone(&pending_approval),
        Arc::clone(&safety),
    );
    let after_tool_call = build_after_tool_call_hook(Arc::clone(&events));
    let stream_events = Arc::clone(&events);
    let stream_seen = Arc::clone(&streamed_text);
    let compaction_for_usage = options.compaction.clone();
    let config = AgentConfig {
        system: Some(crate::prompts::agent_system(&cfg).await),
        tools,
        opts: CompleteOpts {
            model: options.model,
            thinking: options.thinking,
            ..CompleteOpts::default()
        },
        before_tool_call: Some(before_tool_call),
        after_tool_call: Some(after_tool_call),
        on_stream_event: Some(Box::new(move |event| {
            if matches!(event, StreamEvent::TextDelta(_)) {
                stream_seen.store(true, Ordering::Relaxed);
            }
            let _ = stream_events.emit(map_stream_event(event));
        })),
        token_log: options.token_log.map(|path| TokenLogConfig {
            path,
            label: "tui".to_string(),
        }),
        compaction: options.compaction,
        ..AgentConfig::default()
    };

    let services =
        crate::provisioning::build_tool_services(workspace, &cfg, true, false, options.analytics)
            .await;
    let mut tool_session = Session::new(workspace.to_path_buf(), cfg);
    crate::provisioning::provision_session(&mut tool_session, &services);
    let session = Arc::new(tokio::sync::Mutex::new(AgentSession::new(
        provider,
        tool_session,
        config,
    )));

    let mut guard = TerminalGuard::enter()?;
    install_panic_hook();
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut composer = String::new();
    let mut history = ComposerHistory::new(history_entries);
    let mut scroll = TranscriptScroll::default();
    let mut turn = None;
    if let Some(prompt) = options
        .initial_prompt
        .filter(|prompt| !prompt.trim().is_empty())
    {
        history.record(prompt.clone());
        turn = Some(start_turn(
            prompt,
            Arc::clone(&session),
            Arc::clone(&events),
            Arc::clone(&streamed_text),
            compaction_for_usage.clone(),
        ));
    }

    let outcome = run_event_loop(
        &mut terminal,
        &mut composer,
        &mut history,
        &mut scroll,
        options.no_color,
        &view,
        &session,
        &events,
        &pending_approval,
        &streamed_text,
        &compaction_for_usage,
        &mut turn,
        &options.models,
    )
    .await;

    if let Some(active) = turn.take() {
        active.abort();
        let _ = active.await;
    }
    pending_approval
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let show_cursor = terminal.show_cursor();
    guard.restore();
    match outcome {
        Err(error) => Err(error),
        Ok(()) => show_cursor.map_err(Into::into),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    composer: &mut String,
    history: &mut ComposerHistory,
    scroll: &mut TranscriptScroll,
    no_color: bool,
    view: &SharedView,
    session: &Arc<tokio::sync::Mutex<AgentSession>>,
    events: &Arc<SessionEventRouter>,
    pending_approval: &SharedApproval,
    streamed_text: &Arc<AtomicBool>,
    compaction: &Option<CompactionPolicy>,
    turn: &mut Option<tokio::task::JoinHandle<()>>,
    models: &[String],
) -> anyhow::Result<()> {
    loop {
        if turn
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(finished) = turn.take() {
                let _ = finished.await;
            }
        }

        {
            let view = view.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            terminal.draw(|frame| {
                super::render_with_options(
                    &view,
                    composer,
                    frame.area(),
                    frame.buffer_mut(),
                    super::RenderOptions {
                        no_color,
                        scroll_from_bottom: scroll.bottom_offset(),
                    },
                );
            })?;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if accepts_key(key) => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    cancel_turn(turn, events, pending_approval).await;
                    continue;
                }
                if resolve_approval_key(key, view, pending_approval) {
                    continue;
                }
                match key.code {
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        composer.push('\n');
                        history.reset_navigation();
                    }
                    KeyCode::Enter => {
                        let line = std::mem::take(composer);
                        match parse_command(&line) {
                            UiCommand::Prompt(prompt) if !prompt.is_empty() && turn.is_none() => {
                                history.record(prompt.clone());
                                scroll.jump_to_end();
                                *turn = Some(start_turn(
                                    prompt,
                                    Arc::clone(session),
                                    Arc::clone(events),
                                    Arc::clone(streamed_text),
                                    compaction.clone(),
                                ));
                            }
                            UiCommand::Prompt(prompt) if !prompt.is_empty() => {
                                *composer = prompt;
                            }
                            UiCommand::Quit => break,
                            UiCommand::StopSession => {
                                let _ = events.emit(SessionEvent::SessionEnding {
                                    reason: "stopped by local terminal".to_string(),
                                });
                                break;
                            }
                            UiCommand::Interrupt => {
                                if turn.is_some() {
                                    cancel_turn(turn, events, pending_approval).await;
                                } else {
                                    push_notice(view, "no turn is running");
                                }
                            }
                            UiCommand::Clear if turn.is_none() => {
                                session.lock().await.clear();
                                view.lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .clear_transcript();
                            }
                            UiCommand::Usage if turn.is_none() => {
                                let usage = session.lock().await.total_usage().clone();
                                push_notice(
                                    view,
                                    format!(
                                        "input={} output={} cache_read={} cache_write={} cost=${:.4}",
                                        usage.input,
                                        usage.output,
                                        usage.cache_read,
                                        usage.cache_write,
                                        usage.cost.total_usd
                                    ),
                                );
                            }
                            UiCommand::Usage => {
                                push_notice(view, "usage is available after the active turn");
                            }
                            UiCommand::Help => push_notice(view, HELP_TEXT),
                            UiCommand::Model(Some(model)) if turn.is_none() => {
                                session.lock().await.set_model(model.clone());
                                let _ = events.emit(SessionEvent::RuntimeOptionsChanged {
                                    options: model_runtime_options(models, &model),
                                });
                            }
                            UiCommand::Model(None) => {
                                push_notice(view, format!("models: {}", models.join(", ")));
                            }
                            UiCommand::Compact => {
                                push_notice(
                                    view,
                                    "manual compaction is not available in this frontend",
                                );
                            }
                            UiCommand::DetachUnavailable => {
                                push_notice(
                                    view,
                                    "detach is unavailable until daemon reattach support exists; use /quit",
                                );
                            }
                            UiCommand::Unknown(command) => {
                                push_notice(view, format!("unknown command: {command}"));
                            }
                            UiCommand::Clear | UiCommand::Model(Some(_)) => {
                                push_notice(view, "command unavailable while a turn is running");
                            }
                            UiCommand::Prompt(_) => {}
                        }
                    }
                    KeyCode::Backspace => {
                        composer.pop();
                        history.reset_navigation();
                    }
                    KeyCode::Up => {
                        if let Some(previous) = history.previous(composer) {
                            *composer = previous;
                        }
                    }
                    KeyCode::Down => {
                        if let Some(next) = history.next() {
                            *composer = next;
                        }
                    }
                    KeyCode::PageUp => {
                        scroll.page_up(transcript_page_height(terminal)?);
                    }
                    KeyCode::PageDown => {
                        scroll.page_down(transcript_page_height(terminal)?);
                    }
                    KeyCode::Home => {
                        scroll.jump_to_start();
                    }
                    KeyCode::End => {
                        scroll.jump_to_end();
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        composer.push(ch);
                        history.reset_navigation();
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                composer.push_str(&text);
                history.reset_navigation();
            }
            Event::Resize(_, _)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Mouse(_)
            | Event::Key(_) => {}
        }
    }
    Ok(())
}

fn transcript_page_height(
    terminal: &Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<usize> {
    Ok(usize::from(
        terminal
            .size()?
            .height
            .saturating_sub(super::TUI_CHROME_HEIGHT)
            .max(1),
    ))
}

fn accepts_key(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn push_notice(view: &SharedView, text: impl Into<String>) {
    view.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_system_message(text);
}

fn resolve_approval_key(
    key: KeyEvent,
    view: &SharedView,
    pending_approval: &SharedApproval,
) -> bool {
    let allow_always = view
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .active_approval()
        .map(|request| request.allow_always_available);
    let Some(allow_always) = allow_always else {
        return false;
    };
    let KeyCode::Char(ch) = key.code else {
        return true;
    };
    let Some(decision) = approval_from_key(ch, allow_always) else {
        return true;
    };
    if let Some(pending) = pending_approval
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let _ = pending.sender.send(decision);
    }
    true
}

fn start_turn(
    prompt: String,
    session: Arc<tokio::sync::Mutex<AgentSession>>,
    events: Arc<SessionEventRouter>,
    streamed_text: Arc<AtomicBool>,
    compaction: Option<CompactionPolicy>,
) -> tokio::task::JoinHandle<()> {
    streamed_text.store(false, Ordering::Relaxed);
    let _ = events.emit(SessionEvent::UserMessage {
        text: prompt.clone(),
    });
    let _ = events.emit(SessionEvent::TurnStatusChanged {
        status: TurnStatus::Running,
    });
    tokio::spawn(async move {
        let turn = session.lock().await.prompt(prompt).await;
        if !streamed_text.load(Ordering::Relaxed) && !turn.text.is_empty() {
            let _ = events.emit(SessionEvent::AssistantDelta {
                text: turn.text.clone(),
            });
        }
        let _ = events.emit(SessionEvent::AssistantDone {
            outcome: turn_outcome(&turn),
        });
        let _ = events.emit(SessionEvent::ContextUsageChanged {
            usage: context_usage(&turn, compaction.as_ref()),
        });
        let _ = events.emit(SessionEvent::TurnStatusChanged {
            status: TurnStatus::Idle,
        });
    })
}

async fn cancel_turn(
    turn: &mut Option<tokio::task::JoinHandle<()>>,
    events: &Arc<SessionEventRouter>,
    pending_approval: &SharedApproval,
) {
    let Some(active) = turn.take() else {
        return;
    };
    active.abort();
    match active.await {
        // A successful task emits AssistantDone + its terminal status before
        // returning, so there is nothing for the interrupt path to synthesize.
        Ok(()) => return,
        Err(error) if error.is_cancelled() => {}
        Err(error) => {
            let _ = events.emit(SessionEvent::AssistantDone {
                outcome: AssistantOutcome::Errored {
                    context_overflow: false,
                    message: format!("turn task failed: {error}"),
                },
            });
            let _ = events.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Idle,
            });
            return;
        }
    }
    let _ = events.emit(SessionEvent::TurnStatusChanged {
        status: TurnStatus::Cancelling,
    });
    pending_approval
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let _ = events.emit(SessionEvent::AssistantDone {
        outcome: AssistantOutcome::Aborted,
    });
    let _ = events.emit(SessionEvent::TurnStatusChanged {
        status: TurnStatus::Cancelled,
    });
}

fn active_tools(workspace: &Path, cfg: &Config) -> Vec<ToolSchema> {
    tool_facade::active_schemas(workspace, &cfg.prompts.resolved_tool_descriptions)
        .into_iter()
        .map(|schema| ToolSchema {
            name: schema.name,
            description: schema.description,
            input_schema: schema.input_schema,
        })
        .collect()
}

fn build_before_tool_call_hook(
    events: Arc<SessionEventRouter>,
    pending_approval: SharedApproval,
    safety: Arc<SafetyPolicy>,
) -> crate::agent::BeforeHook {
    Box::new(move |info: &ToolCallInfo| {
        let events = Arc::clone(&events);
        let pending_approval = Arc::clone(&pending_approval);
        let safety = Arc::clone(&safety);
        let id = info.id.clone();
        let name = info.name.clone();
        let detail = serde_json::to_string(&info.input).unwrap_or_else(|_| "{}".to_string());
        Box::pin(async move {
            let _ = events.emit(SessionEvent::ToolCallStarted {
                id: id.clone(),
                name: name.clone(),
                title: name.clone(),
                input_summary: Some(detail.clone()),
            });
            let decision = match safety.gate(&name) {
                Gate::Block(reason) => BeforeHookResult::Block(reason),
                Gate::Allow => BeforeHookResult::Allow,
                Gate::NeedsApproval => {
                    let approval_id = uuid::Uuid::new_v4().to_string();
                    let (sender, receiver) = oneshot::channel();
                    let registered = {
                        let mut pending = pending_approval
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if pending.is_some() {
                            false
                        } else {
                            *pending = Some(PendingApproval { sender });
                            true
                        }
                    };
                    if !registered {
                        // The TUI admits one active turn and AgentSession runs
                        // tool calls serially. Treat a violated invariant as a
                        // blocked call instead of silently replacing the first
                        // operator decision channel.
                        BeforeHookResult::Block(
                            "another tool approval is already pending".to_string(),
                        )
                    } else {
                        let _ = events.emit(SessionEvent::TurnStatusChanged {
                            status: TurnStatus::WaitingForApproval,
                        });
                        let _ = events.emit(SessionEvent::ApprovalRequested {
                            request: ApprovalRequest {
                                id: approval_id.clone(),
                                tool_call_id: id.clone(),
                                tool: name.clone(),
                                detail,
                                allow_always_available: true,
                            },
                        });
                        let approval = receiver.await.unwrap_or(ApprovalDecision::Deny);
                        let _ = events.emit(SessionEvent::ApprovalResolved {
                            approval_id,
                            decision: approval,
                            resolved_by: "tui_local".to_string(),
                        });
                        let _ = events.emit(SessionEvent::TurnStatusChanged {
                            status: TurnStatus::Running,
                        });
                        match approval {
                            ApprovalDecision::AllowOnce => BeforeHookResult::Allow,
                            ApprovalDecision::AllowAlways => {
                                safety.remember_always(&name);
                                BeforeHookResult::Allow
                            }
                            ApprovalDecision::Deny => BeforeHookResult::Block(format!(
                                "blocked: operator declined approval for '{name}'"
                            )),
                        }
                    }
                }
            };
            let status = if matches!(decision, BeforeHookResult::Allow) {
                ToolCallStateStatus::InProgress
            } else {
                ToolCallStateStatus::Failed
            };
            let _ = events.emit(SessionEvent::ToolCallUpdated {
                id: id.clone(),
                status,
            });
            if let BeforeHookResult::Block(reason) = &decision {
                let _ = events.emit(SessionEvent::ToolCallFinished {
                    id,
                    ok: false,
                    output: reason.clone(),
                });
            }
            decision
        })
    })
}

fn build_after_tool_call_hook(events: Arc<SessionEventRouter>) -> crate::agent::AfterHook {
    Box::new(move |info, output, is_error| {
        let _ = events.emit(SessionEvent::ToolCallFinished {
            id: info.id.clone(),
            ok: !is_error,
            output: output.to_string(),
        });
        AfterHookResult::Continue
    })
}

fn map_stream_event(event: StreamEvent) -> SessionEvent {
    match event {
        StreamEvent::TextDelta(text) => SessionEvent::AssistantDelta { text },
        StreamEvent::ThinkingDelta(text) => SessionEvent::ThoughtDelta { text },
    }
}

fn turn_outcome(turn: &TurnResult) -> AssistantOutcome {
    match turn.stop_reason {
        StopReason::EndTurn | StopReason::ToolUse => AssistantOutcome::Completed,
        StopReason::Error => AssistantOutcome::Errored {
            context_overflow: turn.context_overflow,
            message: turn
                .error_message
                .clone()
                .unwrap_or_else(|| "provider error".to_string()),
        },
        StopReason::Refusal => AssistantOutcome::Refused,
        StopReason::Aborted => AssistantOutcome::Aborted,
        StopReason::MaxTokens => AssistantOutcome::MaxTokens,
    }
}

fn context_usage(turn: &TurnResult, compaction: Option<&CompactionPolicy>) -> ContextUsage {
    let (window, reservation, high_water) = compaction.map_or((None, 0, None), |policy| {
        (
            Some(policy.context_window),
            policy.output_reservation,
            Some((policy.high_water * policy.budget() as f64) as u64),
        )
    });
    ContextUsage::new(
        turn.last_call_usage.prompt_tokens(),
        window,
        reservation,
        high_water,
    )
}

fn model_runtime_options(models: &[String], current: &str) -> Vec<RuntimeOption> {
    let mut choices = models.to_vec();
    if !choices.iter().any(|model| model == current) {
        choices.insert(0, current.to_string());
    }
    vec![RuntimeOption::select(
        "model",
        "Model",
        RuntimeValue::String(current.to_string()),
        choices
            .into_iter()
            .map(|model| RuntimeChoice::new(model.clone(), model))
            .collect(),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Usage;
    use crate::safety::ApprovalMode;
    use serde_json::json;

    fn turn(stop_reason: StopReason) -> TurnResult {
        TurnResult {
            text: String::new(),
            usage: Usage::default(),
            last_call_usage: Usage::default(),
            stop_reason,
            error_message: None,
            context_overflow: false,
        }
    }

    #[test]
    fn stream_events_use_the_canonical_projection() {
        assert_eq!(
            map_stream_event(StreamEvent::TextDelta("hello".to_string())),
            SessionEvent::AssistantDelta {
                text: "hello".to_string()
            }
        );
        assert_eq!(
            map_stream_event(StreamEvent::ThinkingDelta("hmm".to_string())),
            SessionEvent::ThoughtDelta {
                text: "hmm".to_string()
            }
        );
    }

    #[test]
    fn terminal_turn_outcomes_remain_distinct() {
        assert_eq!(
            turn_outcome(&turn(StopReason::EndTurn)),
            AssistantOutcome::Completed
        );
        assert_eq!(
            turn_outcome(&turn(StopReason::Refusal)),
            AssistantOutcome::Refused
        );
        assert_eq!(
            turn_outcome(&turn(StopReason::Aborted)),
            AssistantOutcome::Aborted
        );
        assert_eq!(
            turn_outcome(&turn(StopReason::MaxTokens)),
            AssistantOutcome::MaxTokens
        );
    }

    #[test]
    fn provider_errors_preserve_overflow_and_message() {
        let mut error = turn(StopReason::Error);
        error.context_overflow = true;
        error.error_message = Some("too large".to_string());

        assert_eq!(
            turn_outcome(&error),
            AssistantOutcome::Errored {
                context_overflow: true,
                message: "too large".to_string()
            }
        );
    }

    #[test]
    fn model_option_includes_current_model_once() {
        let options = model_runtime_options(&["a".to_string(), "b".to_string()], "b");
        let RuntimeValue::String(value) = &options[0].value else {
            panic!("string model value");
        };
        assert_eq!(value, "b");
        let crate::session_protocol::RuntimeOptionSpec::Select { choices } = &options[0].spec
        else {
            panic!("select model option");
        };
        assert_eq!(choices.len(), 2);
    }

    #[tokio::test]
    async fn approval_hook_routes_local_decision_into_canonical_state() {
        let view = Arc::new(StdMutex::new(ViewState::new("session")));
        let handler_view = Arc::clone(&view);
        let events = Arc::new(SessionEventRouter::new(Some(Arc::new(
            move |seq, event| {
                let _ = handler_view
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .apply_event(seq, event);
            },
        ))));
        let pending: SharedApproval = Arc::new(StdMutex::new(None));
        let safety = Arc::new(SafetyPolicy {
            approval_mode: ApprovalMode::Interactive,
            ..SafetyPolicy::default()
        });
        let hook = build_before_tool_call_hook(Arc::clone(&events), Arc::clone(&pending), safety);
        let info = ToolCallInfo {
            id: "call-1".to_string(),
            name: "exec".to_string(),
            input: json!({"command": "true"}),
        };
        let respond = async {
            loop {
                let waiting = pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(waiting) = waiting {
                    waiting.sender.send(ApprovalDecision::AllowOnce).unwrap();
                    break;
                }
                tokio::task::yield_now().await;
            }
        };

        let (decision, ()) = tokio::join!(hook(&info), respond);

        assert!(matches!(decision, BeforeHookResult::Allow));
        let view = view.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(view.active_approval().is_none());
        assert_eq!(view.turn_status(), TurnStatus::Running);
        assert_eq!(view.tool_calls()[0].status, ToolCallStateStatus::InProgress);
    }

    #[tokio::test]
    async fn completed_turn_is_not_relabelled_cancelled() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let handler_seen = Arc::clone(&seen);
        let events = Arc::new(SessionEventRouter::new(Some(Arc::new(
            move |_seq, event| {
                handler_seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        ))));
        let pending: SharedApproval = Arc::new(StdMutex::new(None));
        let mut turn = Some(tokio::spawn(async {}));
        while !turn.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }

        cancel_turn(&mut turn, &events, &pending).await;

        assert!(turn.is_none());
        assert!(seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
    }

    #[tokio::test]
    async fn second_approval_is_blocked_without_replacing_first() {
        let events = Arc::new(SessionEventRouter::default());
        let pending: SharedApproval = Arc::new(StdMutex::new(None));
        let safety = Arc::new(SafetyPolicy {
            approval_mode: ApprovalMode::Paranoid,
            ..SafetyPolicy::default()
        });
        let hook = build_before_tool_call_hook(Arc::clone(&events), Arc::clone(&pending), safety);
        let first_info = ToolCallInfo {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            input: json!({"path": "one"}),
        };
        let second_info = ToolCallInfo {
            id: "call-2".to_string(),
            name: "read_file".to_string(),
            input: json!({"path": "two"}),
        };
        let first = hook(&first_info);
        tokio::pin!(first);
        loop {
            tokio::select! {
                _ = &mut first => panic!("first approval resolved before a response"),
                _ = tokio::task::yield_now() => {
                    if pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .is_some()
                    {
                        break;
                    }
                }
            }
        }

        let second = tokio::time::timeout(Duration::from_millis(50), hook(&second_info))
            .await
            .expect("second approval should not wait");

        assert!(matches!(
            second,
            BeforeHookResult::Block(ref reason) if reason.contains("already pending")
        ));
        let first_pending = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("first approval remains registered");
        first_pending
            .sender
            .send(ApprovalDecision::AllowOnce)
            .unwrap();
        assert!(matches!(first.await, BeforeHookResult::Allow));
    }
}
