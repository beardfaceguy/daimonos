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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::session_protocol::{
    RuntimeOption, RuntimeOptionSpec, RuntimeValue, ToolCallStateStatus, TranscriptRole, TurnStatus,
};
use crate::tui::state::ViewState;

pub const STATUS_HEIGHT: u16 = 1;
pub const COMPOSER_TEXT_HEIGHT: u16 = 4;
pub const COMPOSER_HEIGHT: u16 = COMPOSER_TEXT_HEIGHT + 2;
pub const TUI_CHROME_HEIGHT: u16 = STATUS_HEIGHT + COMPOSER_HEIGHT;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderOptions {
    pub no_color: bool,
    pub scroll_from_bottom: usize,
}

/// Render the whole TUI frame for `state` into `area` of `buf`.
///
/// `composer` is the UI-local input buffer (not part of canonical session
/// state). Layout top-to-bottom: transcript (flex) / status bar (1 row) /
/// composer (4 text rows plus a border).
pub fn render(state: &ViewState, composer: &str, area: Rect, buf: &mut Buffer) {
    render_with_options(state, composer, area, buf, RenderOptions::default());
}

pub fn render_with_options(
    state: &ViewState,
    composer: &str,
    area: Rect,
    buf: &mut Buffer,
    options: RenderOptions,
) {
    let chunks = tui_layout(area);
    render_transcript(state, chunks[0], buf, options.scroll_from_bottom);
    render_status(state, chunks[1], buf);
    render_composer(composer, chunks[2], buf);
    // Overlay last so it sits above every base layer. It is the one thing a
    // human must act on, and ADR-011 keeps approval authority with the local
    // TUI regardless of which client ultimately answers.
    render_approval_modal(state, area, buf);
    if options.no_color {
        for cell in &mut buf.content {
            cell.set_fg(Color::Reset).set_bg(Color::Reset);
        }
    }
}

fn tui_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Length(COMPOSER_HEIGHT),
        ])
        .split(area)
}

fn render_transcript(state: &ViewState, area: Rect, buf: &mut Buffer, scroll_from_bottom: usize) {
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

    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let max_top = paragraph
        .line_count(area.width)
        .saturating_sub(usize::from(area.height));
    let top = max_top.saturating_sub(scroll_from_bottom.min(max_top));
    paragraph
        .scroll((u16::try_from(top).unwrap_or(u16::MAX), 0))
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
    let inner = block.inner(area);
    let (wrapped, viewport) = wrapped_composer(composer, inner);
    let visible = viewport
        .map(|view| {
            wrapped
                .split('\n')
                .skip(view.scroll)
                .take(usize::from(inner.height))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    Paragraph::new(visible).block(block).render(area, buf);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComposerViewport {
    scroll: usize,
    cursor_column: u16,
    cursor_row: u16,
}

/// Terminal cursor position for the insertion point at the end of `composer`.
///
/// The current editor is append/backspace based, so the insertion point is
/// always the end of the buffer. The viewport follows wrapped and explicit
/// lines, keeping that point inside the four-row composer.
pub fn composer_cursor_position(composer: &str, area: Rect) -> Option<(u16, u16)> {
    let composer_area = *tui_layout(area).get(2)?;
    let inner = Block::default().borders(Borders::ALL).inner(composer_area);
    let viewport = composer_viewport(composer, inner)?;
    Some((
        inner.x.saturating_add(viewport.cursor_column),
        inner.y.saturating_add(viewport.cursor_row),
    ))
}

fn composer_viewport(composer: &str, inner: Rect) -> Option<ComposerViewport> {
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let (_, logical_row, column) = wrap_composer(composer, usize::from(inner.width));
    let viewport_height = usize::from(inner.height);
    let scroll = logical_row.saturating_sub(viewport_height.saturating_sub(1));
    Some(ComposerViewport {
        scroll,
        cursor_column: u16::try_from(column)
            .unwrap_or(u16::MAX)
            .min(inner.width.saturating_sub(1)),
        cursor_row: u16::try_from(logical_row.saturating_sub(scroll))
            .unwrap_or(u16::MAX)
            .min(inner.height.saturating_sub(1)),
    })
}

fn wrapped_composer(composer: &str, inner: Rect) -> (String, Option<ComposerViewport>) {
    if inner.width == 0 || inner.height == 0 {
        return (String::new(), None);
    }
    let (wrapped, _, _) = wrap_composer(composer, usize::from(inner.width));
    (wrapped, composer_viewport(composer, inner))
}

fn wrap_composer(composer: &str, width: usize) -> (String, usize, usize) {
    debug_assert!(width > 0);
    let text = sanitize(composer);
    let mut wrapped = String::with_capacity(text.len());
    let mut logical_row = 0usize;
    let mut column = 0usize;
    let mut wrapped_at_boundary = false;

    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            if !wrapped_at_boundary {
                wrapped.push('\n');
                logical_row = logical_row.saturating_add(1);
            }
            column = 0;
            wrapped_at_boundary = false;
            continue;
        }

        let mut grapheme_width = UnicodeWidthStr::width(grapheme);
        if grapheme_width == 0 {
            wrapped.push_str(grapheme);
            wrapped_at_boundary = false;
            continue;
        }
        let rendered_grapheme = if grapheme_width > width {
            grapheme_width = 1;
            "\u{fffd}"
        } else {
            grapheme
        };
        if column > 0 && column.saturating_add(grapheme_width) > width {
            wrapped.push('\n');
            logical_row = logical_row.saturating_add(1);
            column = 0;
        }
        wrapped.push_str(rendered_grapheme);
        column = column.saturating_add(grapheme_width).min(width);
        if column == width {
            wrapped.push('\n');
            logical_row = logical_row.saturating_add(1);
            column = 0;
            wrapped_at_boundary = true;
        } else {
            wrapped_at_boundary = false;
        }
    }
    (wrapped, logical_row, column)
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
                request_id: None,
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

    #[test]
    fn composer_reserves_four_visible_text_rows() {
        assert_eq!(COMPOSER_TEXT_HEIGHT, 4);
        assert_eq!(COMPOSER_HEIGHT, 6);
    }

    #[test]
    fn composer_scrolls_to_keep_latest_input_visible() {
        let state = ViewState::new("sess-1");
        let out = render_to_string(&state, "first\nsecond\nthird\nfourth\nfifth", 40, 12);

        assert!(
            !out.contains("first"),
            "oldest row should scroll out:\n{out}"
        );
        for visible in ["second", "third", "fourth", "fifth"] {
            assert!(
                out.contains(visible),
                "{visible} should remain visible:\n{out}"
            );
        }
    }

    #[test]
    fn composer_cursor_follows_wrapped_text_at_viewport_bottom() {
        let area = Rect::new(0, 0, 12, 12);
        let position =
            composer_cursor_position("1111111111\n2\n3\n4\n5", area).expect("composer cursor");

        // Four text rows inside a six-row bordered composer. The fifth logical
        // row scrolls, leaving the insertion cursor on the last visible row.
        assert_eq!(position, (2, 10));
    }

    #[test]
    fn composer_scrolls_a_single_continuously_wrapped_line() {
        let state = ViewState::new("sess-1");
        let composer = concat!(
            "FIRST11111",
            "SECOND2222",
            "THIRD33333",
            "FOURTH4444",
            "FIFTH55555",
        );
        let out = render_to_string(&state, composer, 12, 12);

        assert!(
            !out.contains("FIRST"),
            "wrapped head should scroll out:\n{out}"
        );
        assert!(
            out.contains("FIFTH"),
            "wrapped tail should stay visible:\n{out}"
        );
    }

    #[test]
    fn composer_cursor_uses_display_width_for_wide_unicode() {
        let area = Rect::new(0, 0, 12, 12);
        // Five double-width glyphs exactly fill the ten-cell inner width,
        // placing the insertion cursor at column zero of the next row.
        assert_eq!(composer_cursor_position("界界界界界", area), Some((1, 8)),);
    }

    #[test]
    fn no_color_mode_resets_every_rendered_cell_color() {
        let mut state = ViewState::new("sess-1");
        state.apply_event(
            1,
            SessionEvent::UserMessage {
                text: "hello".into(),
                request_id: None,
            },
        );
        let area = Rect::new(0, 0, 80, 8);
        let mut buf = Buffer::empty(area);

        render_with_options(
            &state,
            "",
            area,
            &mut buf,
            RenderOptions {
                no_color: true,
                ..RenderOptions::default()
            },
        );

        assert!(buf
            .content
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));
    }

    #[test]
    fn transcript_scroll_moves_away_from_and_back_to_latest_entries() {
        let mut state = ViewState::new("sess-1");
        for index in 1..=6 {
            state.apply_event(
                index,
                SessionEvent::UserMessage {
                    text: format!("message-{index}"),
                    request_id: None,
                },
            );
        }
        let area = Rect::new(0, 0, 40, 8);
        let mut latest = Buffer::empty(area);
        render_with_options(&state, "", area, &mut latest, RenderOptions::default());
        let mut earlier = Buffer::empty(area);
        render_with_options(
            &state,
            "",
            area,
            &mut earlier,
            RenderOptions {
                scroll_from_bottom: usize::MAX,
                ..RenderOptions::default()
            },
        );

        let latest = buffer_to_string(&latest);
        let earlier = buffer_to_string(&earlier);
        assert!(latest.contains("message-6"));
        assert!(!latest.contains("message-1"));
        assert!(earlier.contains("message-1"));
        assert!(!earlier.contains("message-6"));
    }

    #[test]
    fn transcript_scroll_counts_wrapped_rows() {
        let mut state = ViewState::new("sess-1");
        state.apply_event(
            1,
            SessionEvent::UserMessage {
                text: "FIRST-START 111111111111111111111111 FIRST-END".into(),
                request_id: None,
            },
        );
        state.apply_event(
            2,
            SessionEvent::UserMessage {
                text: "SECOND-START 222222222222222222222 SECOND-END".into(),
                request_id: None,
            },
        );
        let area = Rect::new(0, 0, 20, 11);
        let mut latest = Buffer::empty(area);
        render_with_options(&state, "", area, &mut latest, RenderOptions::default());
        let mut earlier = Buffer::empty(area);
        render_with_options(
            &state,
            "",
            area,
            &mut earlier,
            RenderOptions {
                scroll_from_bottom: 2,
                ..RenderOptions::default()
            },
        );

        let latest = buffer_to_string(&latest);
        let earlier = buffer_to_string(&earlier);
        assert_ne!(latest, earlier, "wrapped-row scroll should move the view");
        assert!(earlier.contains("FIRST-START"));
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
    fn composer_wraps_emoji_graphemes_as_single_display_units() {
        let area = Rect::new(0, 0, 6, 12);
        assert_eq!(composer_cursor_position("👩‍🔬👩‍🔬", area), Some((1, 8)),);
    }

    #[test]
    fn composer_keeps_combining_marks_with_their_base_character() {
        let area = Rect::new(0, 0, 6, 12);
        assert_eq!(composer_cursor_position("abc\u{301}d", area), Some((1, 8)),);
    }

    #[test]
    fn composer_scroll_does_not_stop_after_u16_max_rows() {
        let area = Rect::new(0, 0, 12, 12);
        let composer = "x".repeat(700_000);
        assert_eq!(composer_cursor_position(&composer, area), Some((1, 10)));
    }

    #[test]
    fn composer_cursor_is_absent_when_no_inner_cells_exist() {
        assert_eq!(
            composer_cursor_position("draft", Rect::new(0, 0, 1, 4)),
            None,
        );
    }

    #[test]
    fn composer_replaces_graphemes_wider_than_a_tiny_viewport() {
        let state = ViewState::new("sess-1");
        let out = render_to_string(&state, "界a", 3, 12);

        assert!(
            out.contains('\u{fffd}'),
            "replacement should be visible:\n{out}"
        );
        assert!(
            !out.contains('界'),
            "unrenderable glyph should be replaced:\n{out}"
        );
        assert_eq!(
            composer_cursor_position("界a", Rect::new(0, 0, 3, 12)),
            Some((1, 9)),
        );
    }

    #[test]
    fn newline_after_standalone_zero_width_grapheme_is_preserved() {
        let area = Rect::new(0, 0, 6, 12);
        assert_eq!(
            composer_cursor_position("abcd\u{200b}\nq", area),
            Some((2, 9)),
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
