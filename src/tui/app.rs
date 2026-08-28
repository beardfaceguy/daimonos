//! Full-screen terminal client for a daemon-owned agent session (Vikunja #1331).
//!
//! Rendering and input consume only canonical daemon state through
//! [`TuiSession`]. Providers, tools, approvals, persistence, and turn tasks stay
//! in the session daemon.

use std::time::Duration;

use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::Terminal;

use crate::session_controller::{
    ControllerSendError, SessionControllerCommand, SessionControllerHandle,
};
use crate::session_protocol::{ClientCapability, RuntimeOptionSpec, RuntimeValue, TurnStatus};

use super::commands::{approval_from_key, parse_command, UiCommand, HELP_TEXT};
use super::input::{
    apply_scroll_action, ComposerHistory, InputMode, TranscriptScroll, VimScrollKeys,
};
use super::session::{TuiSession, TuiSessionUpdate};
use super::terminal::{install_panic_hook, TerminalGuard};

const RENDER_INTERVAL: Duration = Duration::from_millis(50);

/// Runtime inputs resolved before entering raw terminal mode.
pub struct TuiOptions {
    pub initial_prompt: Option<String>,
    pub no_color: bool,
    pub model_override: Option<String>,
    pub history_entries: usize,
    pub command_timeout: Duration,
    pub controller_factory: Option<super::session::ControllerFactory>,
    pub switch_policy: super::session::SwitchPolicy,
}

/// Attach the full-screen UI to one daemon-owned session.
pub async fn run(controller: SessionControllerHandle, options: TuiOptions) -> anyhow::Result<()> {
    let mut session = TuiSession::attach_with_switching(
        controller,
        options.command_timeout,
        options.controller_factory,
        options.switch_policy,
    )
    .await?;

    if let Some(model) = options.model_override.as_deref() {
        let candidate = RuntimeValue::String(model.to_string());
        let option = session
            .state()
            .runtime_options()
            .iter()
            .find(|option| option.id == "model")
            .ok_or_else(|| anyhow::anyhow!("the daemon does not advertise a model option"))?;
        if !option.accepts(&candidate) {
            anyhow::bail!("model '{model}' is not offered by the running session daemon");
        }
        session
            .set_config("model", candidate)
            .await
            .map_err(|error| anyhow::anyhow!("failed to apply model override: {error}"))?;
    }

    let initial_prompt = options
        .initial_prompt
        .filter(|prompt| !prompt.trim().is_empty());
    if let Some(prompt) = initial_prompt.as_ref() {
        if !is_quiescent(session.state().turn_status()) {
            anyhow::bail!("new daemon session was not idle; initial prompt was not sent");
        }
        session
            .send(SessionControllerCommand::Prompt {
                text: prompt.clone(),
            })
            .await
            .map_err(|error| anyhow::anyhow!("failed to queue initial prompt: {error:?}"))?;
    }

    let mut guard = TerminalGuard::enter()?;
    install_panic_hook();
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut composer = String::new();
    let mut history = ComposerHistory::new(options.history_entries);
    if let Some(prompt) = initial_prompt {
        history.record(prompt);
    }
    let mut scroll = TranscriptScroll::default();
    // run_event_loop owns EventStream. Returning drops its reader before this
    // frame restores cursor/raw/alternate-screen state on every exit path.
    let outcome = run_event_loop(
        &mut terminal,
        &mut composer,
        &mut history,
        &mut scroll,
        options.no_color,
        &mut session,
    )
    .await;

    let show_cursor = terminal.show_cursor();
    guard.restore();
    let stop_result = match &outcome {
        Ok(TuiExit::Stop) => session.stop().await.map_err(|error| {
            anyhow::anyhow!(
                "session stop was not confirmed; the daemon session may still be running: {error}"
            )
        }),
        Ok(TuiExit::Detach) | Err(_) => Ok(()),
    };
    session.shutdown().await;
    match outcome {
        Err(error) => Err(error),
        Ok(_) => {
            stop_result?;
            show_cursor.map_err(Into::into)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiExit {
    Detach,
    Stop,
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    composer: &mut String,
    history: &mut ComposerHistory,
    scroll: &mut TranscriptScroll,
    no_color: bool,
    session: &mut TuiSession,
) -> anyhow::Result<TuiExit> {
    let mut mode = InputMode::Insert;
    let mut vim_keys = VimScrollKeys::default();
    let mut terminal_events = EventStream::new();
    let mut render_tick = tokio::time::interval(RENDER_INTERVAL);
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    render_tick.tick().await;
    draw_tui(terminal, composer, scroll, no_color, session, mode)?;
    loop {
        enum LoopInput {
            Session(TuiSessionUpdate),
            Terminal(std::io::Result<Event>),
            TerminalClosed,
            Render,
        }
        let input = tokio::select! {
            // Human input and the bounded render cadence intentionally outrank
            // model-token updates so a hot session stream cannot starve local
            // control. This is a responsiveness invariant, not a fairness
            // micro-optimization.
            biased;
            event = terminal_events.next() => match event {
                Some(event) => LoopInput::Terminal(event),
                None => LoopInput::TerminalClosed,
            },
            _ = render_tick.tick() => LoopInput::Render,
            update = session.next_update() => LoopInput::Session(update),
        };
        let event = match input {
            LoopInput::Session(update) => {
                match update {
                    TuiSessionUpdate::Updated => {}
                    TuiSessionUpdate::Failed(message) => {
                        anyhow::bail!("{message}");
                    }
                    TuiSessionUpdate::Detached | TuiSessionUpdate::Stopped => {
                        return Ok(TuiExit::Detach);
                    }
                }
                continue;
            }
            LoopInput::Terminal(event) => event?,
            LoopInput::TerminalClosed => return Ok(TuiExit::Detach),
            LoopInput::Render => {
                draw_tui(terminal, composer, scroll, no_color, session, mode)?;
                continue;
            }
        };
        match event {
            Event::Key(key) if accepts_key(key) => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    interrupt(session);
                    draw_tui(terminal, composer, scroll, no_color, session, mode)?;
                    continue;
                }
                if handle_approval_key(key, session) {
                    draw_tui(terminal, composer, scroll, no_color, session, mode)?;
                    continue;
                }
                if key.code == KeyCode::Esc {
                    mode = match mode {
                        InputMode::Insert => InputMode::Scroll,
                        InputMode::Scroll => InputMode::Insert,
                    };
                    vim_keys.reset();
                    draw_tui(terminal, composer, scroll, no_color, session, mode)?;
                    continue;
                }
                if mode == InputMode::Scroll {
                    handle_scroll_key(key, terminal, scroll, &mut mode, &mut vim_keys)?;
                    draw_tui(terminal, composer, scroll, no_color, session, mode)?;
                    continue;
                }
                match key.code {
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        composer.push('\n');
                        history.reset_navigation();
                    }
                    KeyCode::Enter => {
                        let line = std::mem::take(composer);
                        if let Some(exit) =
                            handle_command(parse_command(&line), session, history, scroll)
                        {
                            return Ok(exit);
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
                    KeyCode::PageUp => scroll.page_up(transcript_page_height(terminal)?),
                    KeyCode::PageDown => scroll.page_down(transcript_page_height(terminal)?),
                    KeyCode::Home => scroll.jump_to_start(),
                    KeyCode::End => scroll.jump_to_end(),
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
        draw_tui(terminal, composer, scroll, no_color, session, mode)?;
    }
}

fn draw_tui(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    composer: &str,
    scroll: &TranscriptScroll,
    no_color: bool,
    session: &TuiSession,
    mode: InputMode,
) -> anyhow::Result<()> {
    terminal.draw(|frame| {
        super::render_with_options(
            session.state(),
            composer,
            frame.area(),
            frame.buffer_mut(),
            super::RenderOptions {
                no_color,
                scroll_from_bottom: scroll.bottom_offset(),
                scroll_mode: mode == InputMode::Scroll,
                allow_always_granted: session.has_capability(ClientCapability::ApproveAlways),
            },
        );
        if session.state().active_approval().is_none() {
            if let Some(position) = super::composer_cursor_position(composer, frame.area()) {
                frame.set_cursor_position(position);
            }
        }
    })?;
    Ok(())
}

fn handle_command(
    command: UiCommand,
    session: &mut TuiSession,
    history: &mut ComposerHistory,
    scroll: &mut TranscriptScroll,
) -> Option<TuiExit> {
    let quiescent = is_quiescent(session.state().turn_status());
    match command {
        UiCommand::Prompt(prompt) if !prompt.is_empty() && quiescent => {
            history.record(prompt.clone());
            scroll.jump_to_end();
            queue(
                session,
                SessionControllerCommand::Prompt { text: prompt },
                "prompt",
            );
        }
        UiCommand::Prompt(prompt) if !prompt.is_empty() => {
            notice(session, "a turn is already running; wait or interrupt it");
        }
        UiCommand::Quit | UiCommand::Detach => return Some(TuiExit::Detach),
        UiCommand::StopSession if session.has_capability(ClientCapability::Stop) => {
            return Some(TuiExit::Stop);
        }
        UiCommand::StopSession => {
            notice(session, "stop capability was not granted for this session");
        }
        UiCommand::Interrupt => interrupt(session),
        UiCommand::Help => notice(session, HELP_TEXT),
        UiCommand::Model(Some(model)) if quiescent => {
            let candidate = RuntimeValue::String(model.clone());
            let accepted = session
                .state()
                .runtime_options()
                .iter()
                .find(|option| option.id == "model")
                .is_some_and(|option| option.accepts(&candidate));
            if accepted {
                queue(
                    session,
                    SessionControllerCommand::SetConfig {
                        config_id: "model".to_string(),
                        value: candidate,
                    },
                    "model change",
                );
            } else {
                notice(
                    session,
                    format!("model '{model}' is not offered by the daemon"),
                );
            }
        }
        UiCommand::Model(None) => {
            let models = model_choices(session);
            let text = if models.is_empty() {
                "the daemon advertises no model choices".to_string()
            } else {
                format!("models: {}", models.join(", "))
            };
            notice(session, text);
        }
        UiCommand::Clear if quiescent => {
            queue(
                session,
                SessionControllerCommand::ClearHistory,
                "clear history",
            );
        }
        UiCommand::Usage if quiescent => {
            queue(session, SessionControllerCommand::GetUsage, "usage");
        }
        UiCommand::Clear | UiCommand::Usage => {
            notice(session, "command unavailable while a turn is running");
        }
        UiCommand::Compact => {
            notice(
                session,
                "manual compaction awaits daemon-authoritative protocol support",
            );
        }
        UiCommand::Unknown(command) => notice(session, format!("unknown command: {command}")),
        UiCommand::Model(Some(_)) => {
            notice(session, "command unavailable while a turn is running");
        }
        UiCommand::Prompt(_) => {}
    }
    None
}

fn handle_approval_key(key: KeyEvent, session: &mut TuiSession) -> bool {
    let Some(request) = session.state().active_approval().cloned() else {
        return false;
    };
    let KeyCode::Char(ch) = key.code else {
        return true;
    };
    if let Some(decision) = approval_from_key(ch, session.allow_always_available()) {
        queue(
            session,
            SessionControllerCommand::Approve {
                approval_id: request.id,
                decision,
            },
            "approval",
        );
    }
    true
}

fn interrupt(session: &mut TuiSession) {
    if is_quiescent(session.state().turn_status()) {
        notice(session, "no turn is running");
    } else {
        queue(session, SessionControllerCommand::Interrupt, "interrupt");
    }
}

fn queue(session: &mut TuiSession, command: SessionControllerCommand, operation: &str) {
    if let Err(error) = session.try_send(command) {
        let reason = match error {
            ControllerSendError::Backpressure => "controller queue is full",
            ControllerSendError::Closed => "controller is closed",
            ControllerSendError::SwitchInProgress => "switch_in_progress",
            ControllerSendError::OperationInFlight => "operation already in flight",
        };
        notice(session, format!("{operation} failed: {reason}"));
    }
}

fn notice(session: &mut TuiSession, text: impl Into<String>) {
    session.state_mut().push_system_message(text);
}

fn model_choices(session: &TuiSession) -> Vec<String> {
    session
        .state()
        .runtime_options()
        .iter()
        .find(|option| option.id == "model")
        .and_then(|option| match &option.spec {
            RuntimeOptionSpec::Select { choices } => {
                Some(choices.iter().map(|choice| choice.id.clone()).collect())
            }
            RuntimeOptionSpec::Boolean | RuntimeOptionSpec::Integer { .. } => None,
        })
        .unwrap_or_default()
}

fn is_quiescent(status: TurnStatus) -> bool {
    matches!(status, TurnStatus::Idle | TurnStatus::Cancelled)
}

fn handle_scroll_key(
    key: KeyEvent,
    terminal: &Terminal<CrosstermBackend<std::io::Stdout>>,
    scroll: &mut TranscriptScroll,
    mode: &mut InputMode,
    vim_keys: &mut VimScrollKeys,
) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Char(ch) => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if let Some(action) = vim_keys.interpret(ch, ctrl) {
                let page = transcript_page_height(terminal)?;
                if apply_scroll_action(scroll, action, page) {
                    *mode = InputMode::Insert;
                }
            }
        }
        KeyCode::Up => scroll.line_up(),
        KeyCode::Down => scroll.line_down(),
        KeyCode::PageUp => scroll.page_up(transcript_page_height(terminal)?),
        KeyCode::PageDown => scroll.page_down(transcript_page_height(terminal)?),
        KeyCode::Home => scroll.jump_to_start(),
        KeyCode::End => scroll.jump_to_end(),
        _ => {}
    }
    Ok(())
}

fn transcript_page_height(
    terminal: &Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<usize> {
    let size = terminal.size()?;
    let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    Ok(usize::from(
        super::render::tui_layout(area)[0].height.max(1),
    ))
}

fn accepts_key(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_and_idle_turns_accept_new_prompts() {
        assert!(is_quiescent(TurnStatus::Idle));
        assert!(is_quiescent(TurnStatus::Cancelled));
        assert!(!is_quiescent(TurnStatus::Running));
        assert!(!is_quiescent(TurnStatus::WaitingForApproval));
        assert!(!is_quiescent(TurnStatus::Cancelling));
    }
}
