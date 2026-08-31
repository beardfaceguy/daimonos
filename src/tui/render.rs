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
    AssistantOutcome, DurabilityStatus, RuntimeOption, RuntimeOptionSpec, RuntimeValue,
    TimelineEntryKind, ToolCallStateStatus, TranscriptRole, TurnStatus,
};
use crate::tui::state::ViewState;

pub const STATUS_HEIGHT: u16 = 1;

/// Share of the non-status area given to the composer; the transcript takes
/// the rest. The composer used to be a fixed six rows, which meant a tall
/// terminal grew the transcript and left the input a stub.
pub const COMPOSER_DENOMINATOR: u16 = 3;

/// Smallest composer that can still show anything: two border rows plus one
/// row of text. Below this the pane is decoration, so the split gives way.
pub const MIN_COMPOSER_HEIGHT: u16 = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderOptions {
    pub no_color: bool,
    pub scroll_from_bottom: usize,
    /// True while the TUI is in vim-style scroll mode; draws the `-- SCROLL --`
    /// indicator over the right edge of the status line.
    pub scroll_mode: bool,
    /// Optional capability gate layered with the canonical request policy.
    pub allow_always_granted: bool,
    /// UTF-8 byte offset of the insertion cursor; defaults to the text end.
    pub composer_cursor: Option<usize>,
    /// True when the terminal accepted the Kitty keyboard enhancement, so
    /// modified Enter (Shift-Enter) is actually reported. Drives the composer
    /// hint: Shift-Enter is only advertised when the terminal can deliver it,
    /// while Ctrl-J (a distinct control byte) is always offered as the
    /// universal newline fallback (vikunja #1424).
    pub keyboard_enhanced: bool,
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
    if options.scroll_mode {
        render_scroll_mode_indicator(chunks[1], buf);
    }
    render_composer(
        composer,
        options.composer_cursor.unwrap_or(composer.len()),
        options.keyboard_enhanced,
        chunks[2],
        buf,
    );
    // Overlay last so it sits above every base layer. It is the one thing a
    // human must act on, and ADR-011 keeps approval authority with the local
    // TUI regardless of which client ultimately answers.
    // Over the transcript, not `area`: a modal centred on the whole screen
    // lands on the status line at common terminal heights, and the status is
    // meant to stay readable while an approval is pending. Keeping it inside
    // the transcript also leaves the composer visible.
    render_approval_modal(state, chunks[0], buf, options.allow_always_granted);
    if options.no_color {
        for cell in &mut buf.content {
            cell.set_fg(Color::Reset).set_bg(Color::Reset);
        }
    }
}

/// Split `area` into transcript / status / composer.
///
/// The composer takes one third of everything below the status line and the
/// transcript the other two thirds, so both panes track the terminal instead
/// of the composer sitting at a fixed six rows.
///
/// Rounding favours the composer: it spends two of its rows on a border, so an
/// odd row buys a whole text line there, while the transcript merely shows one
/// line fewer. On a terminal too short to give the composer even
/// [`MIN_COMPOSER_HEIGHT`], the transcript yields rather than the composer
/// collapsing into pure border.
///
/// This is the single source of truth for pane heights. `transcript_page_height`
/// in `app.rs` calls it rather than recomputing from constants — the previous
/// `TUI_CHROME_HEIGHT` was a second, independent description of the same split,
/// and a layout change would silently desync PageUp/PageDown from what is drawn.
pub fn tui_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    let body = area.height.saturating_sub(STATUS_HEIGHT);
    let composer = if body <= MIN_COMPOSER_HEIGHT {
        body
    } else {
        // Round up, so 1/3 of 23 is 8 rather than 7.
        body.div_ceil(COMPOSER_DENOMINATOR).max(MIN_COMPOSER_HEIGHT)
    };

    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Length(composer),
        ])
        .split(area)
}

fn render_transcript(state: &ViewState, area: Rect, buf: &mut Buffer, scroll_from_bottom: usize) {
    let mut lines: Vec<Line> = Vec::new();

    for entry in state.timeline() {
        match &entry.entry {
            TimelineEntryKind::User { text, .. } => {
                push_text_lines(&mut lines, TranscriptRole::User, text);
            }
            TimelineEntryKind::Assistant { text, .. } => {
                push_text_lines(&mut lines, TranscriptRole::Assistant, text);
            }
            TimelineEntryKind::Thought { text, .. } => {
                push_text_lines(&mut lines, TranscriptRole::Thought, text);
            }
            TimelineEntryKind::System { text, .. } => {
                push_text_lines(&mut lines, TranscriptRole::System, text);
            }
            TimelineEntryKind::Outcome { outcome } => {
                if let Some(note) = outcome_render_note(outcome) {
                    push_text_lines(&mut lines, TranscriptRole::System, &note);
                }
            }
            TimelineEntryKind::Tool {
                name,
                title,
                status,
                output,
                ..
            } => {
                let symbol = status_symbol(*status);
                let style = status_style(*status);
                let mut spans = vec![Span::styled(
                    format!("  {symbol} {} ", sanitize(title)),
                    style,
                )];
                spans.push(Span::styled(
                    format!("({})", sanitize(name)),
                    Style::default().fg(Color::DarkGray),
                ));
                lines.push(Line::from(spans));
                if let Some(output) = output {
                    push_text_lines(&mut lines, TranscriptRole::System, output);
                }
            }
        }
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

fn push_text_lines(lines: &mut Vec<Line<'static>>, role: TranscriptRole, raw: &str) {
    let (prefix, style) = role_prefix(role);
    let text = sanitize(raw);
    for (index, part) in text.split('\n').enumerate() {
        if index == 0 {
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::raw(part.to_string()),
            ]));
        } else {
            lines.push(Line::from(format!("    {part}")));
        }
    }
}

fn outcome_render_note(outcome: &AssistantOutcome) -> Option<String> {
    match outcome {
        AssistantOutcome::Completed => None,
        AssistantOutcome::Errored { message, .. } => Some(format!("[turn errored: {message}]")),
        AssistantOutcome::Refused => Some("[turn refused]".to_string()),
        AssistantOutcome::Aborted => Some("[turn interrupted]".to_string()),
        AssistantOutcome::MaxTokens => Some("[turn hit the output token limit]".to_string()),
    }
}

/// Vim's `-- MODE --` habit: a right-aligned indicator on the status line
/// while scroll mode owns the keyboard, doubling as the key cheat-sheet. Drawn
/// after [`render_status`] so it overlays the info text on narrow terminals
/// (the mode is the thing the user must know to get their keys back).
fn render_scroll_mode_indicator(area: Rect, buf: &mut Buffer) {
    const LABEL: &str =
        " -- SCROLL --  j/k \u{b7} C-d/C-u \u{b7} C-f/C-b \u{b7} gg/G \u{b7} Esc/i to type ";
    let width = u16::try_from(LABEL.chars().count())
        .unwrap_or(u16::MAX)
        .min(area.width);
    if width == 0 || area.height == 0 {
        return;
    }
    let rect = Rect {
        x: area.x + (area.width - width),
        y: area.y,
        width,
        height: 1,
    };
    Paragraph::new(Line::from(LABEL))
        .style(Style::default().fg(Color::Black).bg(Color::Yellow))
        .render(rect, buf);
}

fn render_status(state: &ViewState, area: Rect, buf: &mut Buffer) {
    // A pending approval is surfaced by the modal overlay (drawn last in
    // `render`), so the status line stays purely informational here.
    let turn = turn_label(state.turn_status());
    let usage = state
        .context_usage()
        .and_then(|usage| {
            usage.utilization_basis_points.map(|basis_points| {
                let estimate = if usage.estimated { "~" } else { "" };
                format!("ctx {estimate}{}%", basis_points / 100)
            })
        })
        .unwrap_or_else(|| "ctx --".to_string());
    let session = state.session_id();
    let model = current_model(state)
        .map(|m| format!("{m} · "))
        .unwrap_or_default();
    let durability = match state.durability_status() {
        DurabilityStatus::Saved => "",
        DurabilityStatus::Unsaved => " · unsaved",
        DurabilityStatus::Saving => " · saving…",
        DurabilityStatus::Degraded => " · save degraded",
        DurabilityStatus::Superseded => " · persistence superseded",
    };
    let text = format!(" {turn}{durability} · {model}{usage} · session {session} ");
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
fn render_approval_modal(
    state: &ViewState,
    area: Rect,
    buf: &mut Buffer,
    allow_always_granted: bool,
) {
    let Some(approval) = state.active_approval() else {
        return;
    };
    let modal = centered_rect(60, 10, area);
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
    if let Some(deadline) = approval.ineligible_deadline_unix_ms {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let status = if approval.deadline_paused {
            "auto-deny timer paused; original deadline retained".to_string()
        } else {
            let remaining = deadline.saturating_sub(now).div_ceil(1_000);
            format!("auto-deny in ~{remaining}s if no eligible client remains")
        };
        lines.push(Line::from(Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        )));
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
    if approval.allow_always_available && allow_always_granted {
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

fn render_composer(
    composer: &str,
    cursor: usize,
    keyboard_enhanced: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    // Ctrl-J is a distinct control byte that every terminal delivers, so it is
    // always the advertised newline. Shift-Enter is only reported when the
    // terminal accepted the Kitty enhancement, so it is added to the hint only
    // then — never promise a gesture the terminal cannot send (vikunja #1424).
    let newline_hint = if keyboard_enhanced {
        "Shift-Enter/Ctrl-J newline"
    } else {
        "Ctrl-J newline"
    };
    let title =
        format!("message (Enter sends · {newline_hint} · Ctrl-C interrupts · /help for help)");
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    let (wrapped, viewport) = wrapped_composer(composer, cursor, inner);
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

/// Terminal cursor position for an insertion point within `composer`.
pub fn composer_cursor_position_at(
    composer: &str,
    cursor: usize,
    area: Rect,
) -> Option<(u16, u16)> {
    let composer_area = *tui_layout(area).get(2)?;
    let inner = Block::default().borders(Borders::ALL).inner(composer_area);
    let viewport = composer_viewport(composer, cursor, inner)?;
    Some((
        inner.x.saturating_add(viewport.cursor_column),
        inner.y.saturating_add(viewport.cursor_row),
    ))
}

/// Backward-compatible end-of-buffer cursor projection.
pub fn composer_cursor_position(composer: &str, area: Rect) -> Option<(u16, u16)> {
    composer_cursor_position_at(composer, composer.len(), area)
}

fn composer_viewport(composer: &str, cursor: usize, inner: Rect) -> Option<ComposerViewport> {
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let cursor = clamped_grapheme_boundary(composer, cursor);
    let (_, logical_row, column) = wrap_composer(&composer[..cursor], usize::from(inner.width));
    Some(viewport_from_cursor(logical_row, column, inner))
}

fn viewport_from_cursor(logical_row: usize, column: usize, inner: Rect) -> ComposerViewport {
    let viewport_height = usize::from(inner.height);
    let scroll = logical_row.saturating_sub(viewport_height.saturating_sub(1));
    ComposerViewport {
        scroll,
        cursor_column: u16::try_from(column)
            .unwrap_or(u16::MAX)
            .min(inner.width.saturating_sub(1)),
        cursor_row: u16::try_from(logical_row.saturating_sub(scroll))
            .unwrap_or(u16::MAX)
            .min(inner.height.saturating_sub(1)),
    }
}

fn wrapped_composer(
    composer: &str,
    cursor: usize,
    inner: Rect,
) -> (String, Option<ComposerViewport>) {
    if inner.width == 0 || inner.height == 0 {
        return (String::new(), None);
    }
    let width = usize::from(inner.width);
    let (wrapped, _, _) = wrap_composer(composer, width);
    let cursor = clamped_grapheme_boundary(composer, cursor);
    let (_, logical_row, column) = wrap_composer(&composer[..cursor], width);
    (
        wrapped,
        Some(viewport_from_cursor(logical_row, column, inner)),
    )
}

fn clamped_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    if cursor == text.len() {
        return cursor;
    }
    text.grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= cursor)
        .last()
        .unwrap_or(0)
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

    fn render_to_string_with_allow_always(
        state: &ViewState,
        composer: &str,
        w: u16,
        h: u16,
    ) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render_with_options(
            state,
            composer,
            area,
            &mut buf,
            RenderOptions {
                allow_always_granted: true,
                ..RenderOptions::default()
            },
        );
        buffer_to_string(&buf)
    }

    /// vikunja #1424: the composer hint must always offer Ctrl-J (works on every
    /// terminal) and must advertise Shift-Enter ONLY when the terminal accepted
    /// the Kitty enhancement — promising Shift-Enter on Konsole 25.12.3 / xterm,
    /// where it is silently swallowed, is exactly the trap being fixed.
    #[test]
    fn composer_hint_advertises_shift_enter_only_when_enhanced() {
        let state = ViewState::new("sess");
        let area = Rect::new(0, 0, 100, 8);
        let hint = |enhanced: bool| {
            let mut buf = Buffer::empty(area);
            render_with_options(
                &state,
                "",
                area,
                &mut buf,
                RenderOptions {
                    keyboard_enhanced: enhanced,
                    ..RenderOptions::default()
                },
            );
            buffer_to_string(&buf)
        };

        let enhanced = hint(true);
        assert!(
            enhanced.contains("Ctrl-J"),
            "Ctrl-J is the universal fallback and is always shown: {enhanced}"
        );
        assert!(
            enhanced.contains("Shift-Enter"),
            "an enhanced terminal advertises Shift-Enter: {enhanced}"
        );

        let plain = hint(false);
        assert!(
            plain.contains("Ctrl-J"),
            "Ctrl-J is shown regardless of enhancement: {plain}"
        );
        assert!(
            !plain.contains("Shift-Enter"),
            "an unsupported terminal must not promise Shift-Enter: {plain}"
        );
    }

    /// Screen row of the composer's Nth text row, derived from the live layout.
    ///
    /// These cursor tests assert an absolute screen position, which depends on
    /// where the composer sits. That is now proportional to terminal height, so
    /// computing it here keeps them testing grapheme handling rather than
    /// silently re-testing the layout constants.
    fn composer_text_row(area: Rect, row: u16) -> u16 {
        tui_layout(area)[2].y + 1 + row
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
    fn status_bar_marks_cross_model_context_estimates() {
        let mut state = ViewState::new("sess");
        state.apply_event(
            1,
            SessionEvent::ContextUsageChanged {
                usage: ContextUsage::new(50, Some(100), 0, None).mark_estimated(),
            },
        );

        let out = render_to_string(&state, "", 60, 12);
        assert!(out.contains("ctx ~50%"), "estimate marker missing:\n{out}");
    }

    #[test]
    fn status_bar_persistently_surfaces_durability_failure() {
        let mut state = ViewState::new("sess");
        state.apply_event(
            1,
            SessionEvent::DurabilityStatusChanged {
                status: DurabilityStatus::Degraded,
            },
        );

        let out = render_to_string(&state, "", 60, 12);
        assert!(
            out.contains("save degraded"),
            "durability warning missing:\n{out}"
        );
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

    /// The composer is one third of the space below the status line, and both
    /// panes scale with the terminal. Fixed heights are what this replaced.
    #[test]
    fn panes_split_two_thirds_transcript_one_third_composer() {
        for height in [24u16, 30, 50, 60] {
            let chunks = tui_layout(Rect::new(0, 0, 80, height));
            let (transcript, status, composer) = (chunks[0], chunks[1], chunks[2]);

            assert_eq!(status.height, STATUS_HEIGHT, "status is a single row");
            assert_eq!(
                transcript.height + status.height + composer.height,
                height,
                "panes must tile the full area at height {height}"
            );

            let body = height - STATUS_HEIGHT;
            // Integer rounding, so allow the odd row either way.
            assert!(
                composer.height.abs_diff(body / 3) <= 1,
                "composer {} is not ~1/3 of {body} at height {height}",
                composer.height
            );
            assert!(
                transcript.height > composer.height,
                "transcript {} should exceed composer {} at height {height}",
                transcript.height,
                composer.height
            );
        }
    }

    /// A composer shorter than its own border is worse than a short transcript:
    /// it shows nothing at all. The transcript gives way first.
    #[test]
    fn tiny_terminal_keeps_the_composer_usable() {
        let chunks = tui_layout(Rect::new(0, 0, 40, 6));
        assert!(
            chunks[2].height >= MIN_COMPOSER_HEIGHT,
            "composer collapsed to {} rows",
            chunks[2].height
        );
        assert_eq!(chunks[0].height + chunks[1].height + chunks[2].height, 6);
    }

    #[test]
    fn composer_scrolls_to_keep_latest_input_visible() {
        let state = ViewState::new("sess-1");
        let out = render_to_string(&state, "first\nsecond\nthird\nfourth\nfifth", 40, 19);

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
    fn composer_cursor_position_tracks_middle_insertion_point() {
        let area = Rect::new(0, 0, 20, 19);
        assert_eq!(
            composer_cursor_position_at("ab界cd", 2, area),
            Some((3, composer_text_row(area, 0)))
        );
        assert_eq!(
            composer_cursor_position_at("ab界cd", "ab界".len(), area),
            Some((5, composer_text_row(area, 0)))
        );
    }

    #[test]
    fn composer_cursor_projection_matches_wrapping_and_sanitization() {
        let narrow = Rect::new(0, 0, 8, 19);
        assert_eq!(
            composer_cursor_position_at("12345界", 5, narrow),
            Some((6, composer_text_row(narrow, 0)))
        );

        let wide = Rect::new(0, 0, 20, 19);
        assert_eq!(
            composer_cursor_position_at("a\tb\u{7}c", 2, wide),
            Some((6, composer_text_row(wide, 0)))
        );
        assert_eq!(
            composer_cursor_position_at("a👩‍🔬b", 2, wide),
            Some((2, composer_text_row(wide, 0))),
            "defensive projection must snap inside-cluster offsets backward"
        );
    }

    #[test]
    fn composer_viewport_follows_cursor_instead_of_text_end() {
        let state = ViewState::new("sess-1");
        let composer = "first\nsecond\nthird\nfourth\nfifth";
        let area = Rect::new(0, 0, 40, 19);
        let mut buf = Buffer::empty(area);
        render_with_options(
            &state,
            composer,
            area,
            &mut buf,
            RenderOptions {
                composer_cursor: Some(0),
                ..RenderOptions::default()
            },
        );
        let rendered = buffer_to_string(&buf);
        assert!(rendered.contains("first"));
        assert!(!rendered.contains("fifth"));
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
        let area = Rect::new(0, 0, 12, 19);
        // Five double-width glyphs exactly fill the ten-cell inner width,
        // placing the insertion cursor at column zero of the next row.
        assert_eq!(
            composer_cursor_position("界界界界界", area),
            Some((1, composer_text_row(area, 1))),
        );
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
    fn scroll_mode_indicator_appears_only_in_scroll_mode() {
        let state = ViewState::new("sess-1");
        let area = Rect::new(0, 0, 80, 12);
        let mut plain = Buffer::empty(area);
        render_with_options(&state, "", area, &mut plain, RenderOptions::default());
        let mut scrolling = Buffer::empty(area);
        render_with_options(
            &state,
            "",
            area,
            &mut scrolling,
            RenderOptions {
                scroll_mode: true,
                ..RenderOptions::default()
            },
        );
        assert!(!buffer_to_string(&plain).contains("-- SCROLL --"));
        assert!(buffer_to_string(&scrolling).contains("-- SCROLL --"));
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
        let area = Rect::new(0, 0, 20, 9);
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
                    ineligible_deadline_unix_ms: None,
                    deadline_paused: false,
                },
            },
        );
        state
    }

    #[test]
    fn approval_modal_renders_detail_and_options() {
        let state = approval_state(true, "rm -rf /tmp/x");
        let out = render_to_string_with_allow_always(&state, "", 72, 18);
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
    fn approval_modal_shows_paused_anchored_deadline() {
        let mut state = approval_state(false, "inspect");
        state.apply_event(
            2,
            SessionEvent::ApprovalDeadlineChanged {
                approval_id: "a1".into(),
                ineligible_deadline_unix_ms: 123_456,
                paused: true,
            },
        );
        let rendered = render_to_string(&state, "", 100, 30);
        assert!(rendered.contains("auto-deny timer paused"));
        assert!(rendered.contains("original deadline retained"));
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
    fn approval_modal_hides_allow_always_without_client_capability() {
        let state = approval_state(true, "touch x");
        let out = render_to_string(&state, "", 72, 18);
        assert!(out.contains("[y]"));
        assert!(
            !out.contains("[a]"),
            "allow-always must require both host policy and client capability:\n{out}"
        );
    }

    #[test]
    fn composer_wraps_emoji_graphemes_as_single_display_units() {
        let area = Rect::new(0, 0, 6, 19);
        assert_eq!(
            composer_cursor_position("👩‍🔬👩‍🔬", area),
            Some((1, composer_text_row(area, 1))),
        );
    }

    #[test]
    fn composer_keeps_combining_marks_with_their_base_character() {
        let area = Rect::new(0, 0, 6, 19);
        assert_eq!(
            composer_cursor_position("abc\u{301}d", area),
            Some((1, composer_text_row(area, 1))),
        );
    }

    #[test]
    fn composer_scroll_does_not_stop_after_u16_max_rows() {
        let area = Rect::new(0, 0, 12, 19);
        let composer = "x".repeat(700_000);
        assert_eq!(
            composer_cursor_position(&composer, area),
            Some((1, composer_text_row(area, 3)))
        );
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
        let out = render_to_string(&state, "界a", 3, 19);

        assert!(
            out.contains('\u{fffd}'),
            "replacement should be visible:\n{out}"
        );
        assert!(
            !out.contains('界'),
            "unrenderable glyph should be replaced:\n{out}"
        );
        // Height 19 reproduces the four-row composer interior this was written
        // against; the row is derived so a future layout change cannot silently
        // turn this grapheme test into a layout test.
        let area = Rect::new(0, 0, 3, 19);
        assert_eq!(
            composer_cursor_position("界a", area),
            Some((1, composer_text_row(area, 2))),
        );
    }

    #[test]
    fn newline_after_standalone_zero_width_grapheme_is_preserved() {
        let area = Rect::new(0, 0, 6, 19);
        assert_eq!(
            composer_cursor_position("abcd\u{200b}\nq", area),
            Some((2, composer_text_row(area, 2))),
        );
    }

    #[test]
    fn newline_after_exact_width_starts_one_empty_logical_line() {
        let area = Rect::new(0, 0, 6, 19);
        // The hard-wrap and explicit newline identify the same next row; they
        // must not create a second blank row.
        assert_eq!(
            composer_cursor_position("abcd\n", area),
            Some((1, composer_text_row(area, 1))),
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
