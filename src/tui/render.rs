//! Pure rendering layer (Vikunja #1091, layer 3).
//!
//! [`render`] is a *pure* projection of a [`ViewState`] (plus the UI-local
//! composer text) onto a ratatui [`Buffer`]. It performs no I/O and reads no
//! global state, so it is fully exercised by `Buffer`/`TestBackend` snapshot
//! tests without a real terminal (ADR-011 verification gate #2).
//!
//! Security invariant (ADR-011 / terminal-correctness): all model- and
//! tool-derived text is passed through [`sanitize`], which strips C0/C1
//! control bytes (including ESC `0x1b`) so tool output can never inject
//! terminal control/OSC sequences into the host terminal.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

use crate::session_protocol::{
    RuntimeOption, RuntimeOptionSpec, RuntimeValue, ToolCallStateStatus, TranscriptRole, TurnStatus,
};
use crate::tui::state::ViewState;

/// Render the whole TUI frame for `state` into `area` of `buf`.
///
/// `composer` is the UI-local input buffer (not part of canonical session
/// state). Layout top-to-bottom: transcript (flex) / status bar (1 row) /
/// composer (3 rows, bordered).
pub fn render(state: &ViewState, composer: &str, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);
    render_transcript(state, chunks[0], buf);
    render_status(state, chunks[1], buf);
    render_composer(composer, chunks[2], buf);
    // Overlay last so it sits above every base layer. It is the one thing a
    // human must act on, and ADR-011 keeps approval authority with the local
    // TUI regardless of which client ultimately answers.
    render_approval_modal(state, area, buf);
}

fn render_transcript(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let mut lines: Vec<Line> = Vec::new();

    for entry in state.transcript() {
        let (prefix, style) = role_prefix(entry.role);
        let text = sanitize(&entry.text);
        let mut first = true;
        // Split on newlines ourselves so continuation lines stay aligned and a
        // stray '\n' can never smuggle a control sequence past the sanitizer.
        for part in text.split('\n') {
            if first {
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::raw(part.to_string()),
                ]));
                first = false;
            } else {
                lines.push(Line::from(format!("    {part}")));
            }
        }
    }

    for call in state.tool_calls() {
        let symbol = status_symbol(call.status);
        let style = status_style(call.status);
        let mut spans = vec![Span::styled(
            format!("  {symbol} {} ", sanitize(&call.title)),
            style,
        )];
        spans.push(Span::styled(
            format!("({})", sanitize(&call.name)),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::from(spans));
    }

    Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn render_status(state: &ViewState, area: Rect, buf: &mut Buffer) {
    // A pending approval is surfaced by the modal overlay (drawn last in
    // `render`), so the status line stays purely informational here.
    let turn = turn_label(state.turn_status());
    let usage = state
        .context_usage()
        .and_then(|u| u.utilization_basis_points)
        .map(|bp| format!("ctx {}%", bp / 100))
        .unwrap_or_else(|| "ctx --".to_string());
    let session = state.session_id();
    let model = current_model(state)
        .map(|m| format!("{m} · "))
        .unwrap_or_default();
    let text = format!(" {turn} · {model}{usage} · session {session} ");
    Paragraph::new(Line::from(sanitize(&text)))
        .style(Style::default().fg(Color::White).bg(Color::Blue))
        .render(area, buf);
}

/// Centered sub-rectangle of `area`, clamped so it never exceeds the frame.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

/// Human-readable current value of a runtime option (resolves a `Select`
/// option's chosen id to its label).
fn option_display(option: &RuntimeOption) -> String {
    let raw = match &option.value {
        RuntimeValue::String(s) => s.clone(),
        RuntimeValue::Bool(b) => b.to_string(),
        RuntimeValue::Integer(i) => i.to_string(),
    };
    if let RuntimeOptionSpec::Select { choices } = &option.spec {
        if let Some(choice) = choices.iter().find(|c| c.id == raw) {
            return choice.label.clone();
        }
    }
    raw
}

fn current_model(state: &ViewState) -> Option<String> {
    state
        .runtime_options()
        .iter()
        .find(|o| o.id == "model")
        .map(option_display)
}

/// Centered modal for the active approval request. No-op when none is pending.
fn render_approval_modal(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let Some(approval) = state.active_approval() else {
        return;
    };
    let modal = centered_rect(60, 9, area);
    // Clear the cells under the modal so the transcript does not bleed through.
    Clear.render(modal, buf);

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("tool: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                sanitize(&approval.tool),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(String::new()),
    ];
    for part in sanitize(&approval.detail).split('\n') {
        lines.push(Line::from(part.to_string()));
    }
    lines.push(Line::from(String::new()));

    let mut options = vec![
        Span::styled(
            "[y]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" allow once   "),
        Span::styled(
            "[n]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" deny"),
    ];
    if approval.allow_always_available {
        options.push(Span::raw("   "));
        options.push(Span::styled(
            "[a]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        options.push(Span::raw(" allow always"));
    }
    lines.push(Line::from(options));

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Approval required")
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .render(modal, buf);
}

fn render_composer(composer: &str, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("message (Enter to send · Ctrl-C interrupts · /help for help)");
    Paragraph::new(Line::from(sanitize(composer)))
        .block(block)
        .wrap(Wrap { trim: false })
        .render(area, buf);
}

fn role_prefix(role: TranscriptRole) -> (&'static str, Style) {
    match role {
        TranscriptRole::User => (
            "you  ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        TranscriptRole::Assistant => (
            "asst ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        TranscriptRole::System => ("sys  ", Style::default().fg(Color::Magenta)),
        TranscriptRole::Thought => (
            "…    ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    }
}

fn turn_label(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Idle => "idle",
        TurnStatus::Running => "running…",
        TurnStatus::WaitingForApproval => "waiting for approval",
        TurnStatus::Cancelling => "cancelling…",
        TurnStatus::Cancelled => "cancelled",
    }
}

fn status_symbol(status: ToolCallStateStatus) -> &'static str {
    match status {
        ToolCallStateStatus::Pending => "○",
        ToolCallStateStatus::InProgress => "◐",
        ToolCallStateStatus::Completed => "✓",
        ToolCallStateStatus::Failed => "✗",
        ToolCallStateStatus::Cancelled => "⊘",
    }
}

fn status_style(status: ToolCallStateStatus) -> Style {
    let color = match status {
        ToolCallStateStatus::Pending => Color::DarkGray,
        ToolCallStateStatus::InProgress => Color::Yellow,
        ToolCallStateStatus::Completed => Color::Green,
        ToolCallStateStatus::Failed => Color::Red,
        ToolCallStateStatus::Cancelled => Color::DarkGray,
    };
    Style::default().fg(color)
}

/// Neutralize terminal control sequences from untrusted (model/tool) text.
///
/// Keeps `'\n'` (the caller splits on it) and expands `'\t'` to spaces; every
/// other control char — crucially ESC (`0x1b`), which begins CSI/OSC escape
/// sequences — is dropped. Without this, a tool that echoed raw ANSI could
/// move the host cursor, clear the screen, or set the window title.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_protocol::{ContextUsage, SessionEvent, ToolCallStateStatus};
    use crate::tui::state::ViewState;
    use ratatui::layout::Rect;

    fn buffer_to_string(buf: &Buffer) -> String {
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn render_to_string(state: &ViewState, composer: &str, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render(state, composer, area, &mut buf);
        buffer_to_string(&buf)
    }

    #[test]
    fn sanitize_strips_escape_sequences() {
        // A classic hostile OSC that would set the terminal title, plus a CSI
        // clear-screen. Neither ESC survives.
        let hostile = "hi\x1b]0;pwned\x07\x1b[2Jthere";
        let clean = sanitize(hostile);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
        assert_eq!(clean, "hi]0;pwned[2Jthere");
    }

    #[test]
    fn sanitize_keeps_newline_and_expands_tab() {
        assert_eq!(sanitize("a\tb\nc"), "a    b\nc");
    }

    #[test]
    fn renders_user_and_assistant_transcript() {
        let mut state = ViewState::new("sess-1");
        state.apply_event(
            1,
            SessionEvent::UserMessage {
                text: "hello".into(),
            },
        );
        state.apply_event(
            2,
            SessionEvent::AssistantDelta {
                text: "hi there".into(),
            },
        );
        let out = render_to_string(&state, "", 40, 12);
        assert!(out.contains("you"), "user prefix missing:\n{out}");
        assert!(out.contains("hello"), "user text missing:\n{out}");
        assert!(out.contains("asst"), "assistant prefix missing:\n{out}");
        assert!(out.contains("hi there"), "assistant text missing:\n{out}");
    }

    #[test]
    fn renders_tool_card_with_status_symbol() {
        let mut state = ViewState::new("sess-1");
        state.apply_event(
            1,
            SessionEvent::ToolCallStarted {
                id: "t1".into(),
                name: "read_file".into(),
                title: "Read src/main.rs".into(),
                input_summary: None,
            },
        );
        state.apply_event(
            2,
            SessionEvent::ToolCallUpdated {
                id: "t1".into(),
                status: ToolCallStateStatus::InProgress,
            },
        );
        let out = render_to_string(&state, "", 50, 12);
        assert!(
            out.contains("Read src/main.rs"),
            "tool title missing:\n{out}"
        );
        assert!(out.contains("read_file"), "tool name missing:\n{out}");
        assert!(out.contains('◐'), "in-progress symbol missing:\n{out}");
    }

    #[test]
    fn status_bar_shows_turn_and_context() {
        let mut state = ViewState::new("sess-xyz");
        state.apply_event(
            1,
            SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            },
        );
        state.apply_event(
            2,
            SessionEvent::ContextUsageChanged {
                usage: ContextUsage::new(100_000, Some(200_000), 4096, None),
            },
        );
        let out = render_to_string(&state, "", 60, 12);
        assert!(out.contains("running"), "turn label missing:\n{out}");
        assert!(out.contains("ctx"), "ctx usage missing:\n{out}");
        assert!(out.contains("sess-xyz"), "session id missing:\n{out}");
    }

    #[test]
    fn pending_approval_shows_modal_over_informational_status() {
        // The modal is the approval surface; the status line stays
        // informational (session id still visible) rather than being hijacked.
        let state = approval_state(true, "rm -rf /tmp/x");
        let out = render_to_string(&state, "", 72, 22);
        assert!(out.contains("Approval required"), "modal missing:\n{out}");
        assert!(out.contains("exec"), "approval tool missing:\n{out}");
        assert!(
            out.contains("sess-1"),
            "status line should still show the session id:\n{out}"
        );
    }

    #[test]
    fn hostile_tool_output_cannot_inject_escape() {
        let mut state = ViewState::new("sess-1");
        state.apply_event(
            1,
            SessionEvent::ToolCallStarted {
                id: "t1".into(),
                name: "exec".into(),
                title: "run \x1b[2Jclear".into(),
                input_summary: None,
            },
        );
        let out = render_to_string(&state, "", 50, 12);
        assert!(!out.contains('\x1b'), "escape leaked into rendered buffer");
    }

    #[test]
    fn composer_text_is_rendered() {
        let state = ViewState::new("sess-1");
        let out = render_to_string(&state, "draft message", 40, 8);
        assert!(
            out.contains("draft message"),
            "composer text missing:\n{out}"
        );
    }

    #[test]
    fn composer_title_points_to_help() {
        let state = ViewState::new("sess-1");
        let out = render_to_string(&state, "", 80, 8);

        assert!(out.contains("/help for help"), "help hint missing:\n{out}");
    }

    fn approval_state(allow_always: bool, detail: &str) -> ViewState {
        use crate::session_protocol::ApprovalRequest;
        let mut state = ViewState::new("sess-1");
        state.apply_event(
            1,
            SessionEvent::ApprovalRequested {
                request: ApprovalRequest {
                    id: "a1".into(),
                    tool_call_id: "t1".into(),
                    tool: "exec".into(),
                    detail: detail.into(),
                    allow_always_available: allow_always,
                },
            },
        );
        state
    }

    #[test]
    fn approval_modal_renders_detail_and_options() {
        let state = approval_state(true, "rm -rf /tmp/x");
        let out = render_to_string(&state, "", 72, 18);
        assert!(
            out.contains("Approval required"),
            "modal title missing:\n{out}"
        );
        assert!(out.contains("exec"), "tool missing:\n{out}");
        assert!(out.contains("rm -rf /tmp/x"), "detail missing:\n{out}");
        assert!(out.contains("[y]"), "allow-once key missing:\n{out}");
        assert!(out.contains("[n]"), "deny key missing:\n{out}");
        assert!(out.contains("[a]"), "allow-always key missing:\n{out}");
    }

    #[test]
    fn approval_modal_hides_allow_always_when_gated() {
        let state = approval_state(false, "touch x");
        let out = render_to_string(&state, "", 72, 18);
        assert!(out.contains("[y]"), "allow-once key missing:\n{out}");
        assert!(
            !out.contains("[a]"),
            "allow-always must be hidden when host-gated:\n{out}"
        );
    }

    #[test]
    fn approval_modal_sanitizes_detail() {
        let state = approval_state(true, "danger \x1b[2J wipe");
        let out = render_to_string(&state, "", 72, 18);
        assert!(!out.contains('\x1b'), "escape leaked through modal:\n{out}");
    }

    #[test]
    fn status_bar_shows_current_model() {
        use crate::session_protocol::{RuntimeChoice, RuntimeOption, RuntimeValue};
        let mut state = ViewState::new("sess-1");
        let option = RuntimeOption::select(
            "model",
            "Model",
            RuntimeValue::String("opus-5".into()),
            vec![RuntimeChoice::new("opus-5", "Claude Opus 5")],
        );
        state.apply_event(
            1,
            SessionEvent::RuntimeOptionsChanged {
                options: vec![option],
            },
        );
        let out = render_to_string(&state, "", 72, 12);
        assert!(
            out.contains("Claude Opus 5"),
            "resolved model label missing from status bar:\n{out}"
        );
    }
}
