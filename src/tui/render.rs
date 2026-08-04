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
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::session_protocol::{ToolCallStateStatus, TranscriptRole, TurnStatus};
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
    // Approval need dominates the status line — it is the one thing a human
    // must act on. Full modal overlay is phase 4; this is the banner.
    if let Some(approval) = state.active_approval() {
        let text = format!(" APPROVAL NEEDED · {} — press a to review ", approval.tool);
        Paragraph::new(Line::from(sanitize(&text)))
            .style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .render(area, buf);
        return;
    }

    let turn = turn_label(state.turn_status());
    let usage = state
        .context_usage()
        .and_then(|u| u.utilization_basis_points)
        .map(|bp| format!("ctx {}%", bp / 100))
        .unwrap_or_else(|| "ctx --".to_string());
    let session = state.session_id();
    let text = format!(" {turn} · {usage} · session {session} ");
    Paragraph::new(Line::from(sanitize(&text)))
        .style(Style::default().fg(Color::White).bg(Color::Blue))
        .render(area, buf);
}

fn render_composer(composer: &str, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("message (Enter to send · Ctrl-C interrupts)");
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
    fn status_bar_prioritizes_pending_approval() {
        use crate::session_protocol::ApprovalRequest;
        let mut state = ViewState::new("sess-1");
        let request = ApprovalRequest {
            id: "a1".into(),
            tool_call_id: "t1".into(),
            tool: "exec".into(),
            detail: "rm -rf /tmp/x".into(),
            allow_always_available: true,
        };
        state.apply_event(1, SessionEvent::ApprovalRequested { request });
        let out = render_to_string(&state, "", 60, 12);
        assert!(
            out.contains("APPROVAL NEEDED"),
            "approval banner missing:\n{out}"
        );
        assert!(out.contains("exec"), "approval tool missing:\n{out}");
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
}
