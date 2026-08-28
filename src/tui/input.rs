//! Pure process-local input state for the interactive TUI.

use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
    /// Retained across vertical moves through short lines; horizontal moves
    /// and edits deliberately reset it, matching conventional editors.
    preferred_column: Option<usize>,
}

impl Composer {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn replace(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.preferred_column = None;
        std::mem::take(&mut self.text)
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.preferred_column = None;
    }

    pub fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.preferred_column = None;
    }

    pub fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = index;
        }
        self.preferred_column = None;
    }

    pub fn move_right(&mut self) {
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
        self.preferred_column = None;
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
        self.preferred_column = None;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    pub fn move_up(&mut self) -> bool {
        let current_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        if current_start == 0 {
            return false;
        }
        let target_column = self
            .preferred_column
            .unwrap_or_else(|| composer_display_width(&self.text[current_start..self.cursor]));
        let previous_end = current_start - 1;
        let previous_start = self.text[..previous_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = previous_start
            + byte_at_display_column(&self.text[previous_start..previous_end], target_column);
        self.preferred_column = Some(target_column);
        true
    }

    pub fn move_down(&mut self) -> bool {
        let current_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let Some(relative_end) = self.text[self.cursor..].find('\n') else {
            return false;
        };
        let current_end = self.cursor + relative_end;
        let target_column = self
            .preferred_column
            .unwrap_or_else(|| composer_display_width(&self.text[current_start..self.cursor]));
        let next_start = current_end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |index| next_start + index);
        self.cursor =
            next_start + byte_at_display_column(&self.text[next_start..next_end], target_column);
        self.preferred_column = Some(target_column);
        true
    }

    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.move_left();
        if self.cursor < end {
            self.text.drain(self.cursor..end);
        }
        self.preferred_column = None;
    }

    pub fn delete(&mut self) {
        let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() else {
            return;
        };
        self.text.drain(self.cursor..self.cursor + grapheme.len());
        self.preferred_column = None;
    }
}

fn composer_display_width(text: &str) -> usize {
    text.graphemes(true)
        .map(|grapheme| {
            if grapheme == "\t" {
                4
            } else if grapheme.chars().all(char::is_control) {
                0
            } else {
                UnicodeWidthStr::width(grapheme)
            }
        })
        .sum()
}

fn byte_at_display_column(line: &str, target: usize) -> usize {
    let mut column = 0usize;
    for (index, grapheme) in line.grapheme_indices(true) {
        let next = column.saturating_add(composer_display_width(grapheme));
        if next > target {
            return index;
        }
        if next == target {
            return index + grapheme.len();
        }
        column = next;
    }
    line.len()
}

pub struct ComposerHistory {
    entries: VecDeque<String>,
    max_entries: usize,
    position: Option<usize>,
    draft: String,
}

impl ComposerHistory {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: max_entries.max(1),
            position: None,
            draft: String::new(),
        }
    }

    pub fn record(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return;
        }
        if self.entries.back() != Some(&prompt) {
            if self.entries.len() >= self.max_entries {
                self.entries.pop_front();
            }
            self.entries.push_back(prompt);
        }
        self.reset_navigation();
    }

    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let position = match self.position {
            Some(position) => position.saturating_sub(1),
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
        };
        self.position = Some(position);
        self.entries.get(position).cloned()
    }

    pub fn next(&mut self) -> Option<String> {
        let position = self.position?;
        if position + 1 < self.entries.len() {
            self.position = Some(position + 1);
            return self.entries.get(position + 1).cloned();
        }
        self.position = None;
        Some(std::mem::take(&mut self.draft))
    }

    pub fn reset_navigation(&mut self) {
        self.position = None;
        self.draft.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Which pane owns the keyboard, vim-style. `Insert` types into the composer
/// (the default, matching the historical TUI); `Scroll` hands the keys to the
/// transcript for vim motions. Esc toggles between them in `app.rs`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    Insert,
    Scroll,
}

/// One vim-style scroll motion over the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    ExitScroll,
}

/// Interpreter for vim scrolling keys while the TUI is in scroll mode.
///
/// Mapping copies vim's window-scrolling commands: `j`/`k` one line,
/// `Ctrl-E`/`Ctrl-Y` one line (cursorless scroll), `Ctrl-D`/`Ctrl-U` half a
/// page, `Ctrl-F`/`Ctrl-B` a full page, `gg` top, `G` bottom. `i` (or `q`)
/// leaves scroll mode, mirroring normal→insert. Tracks the pending `g` so
/// `gg` works as a chord; any non-`g` key cancels the chord and is swallowed
/// (a cancelled chord never doubles as its own motion).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VimScrollKeys {
    pending_g: bool,
}

impl VimScrollKeys {
    pub fn interpret(&mut self, ch: char, ctrl: bool) -> Option<ScrollAction> {
        if ctrl {
            self.pending_g = false;
            return match ch {
                'e' => Some(ScrollAction::LineDown),
                'y' => Some(ScrollAction::LineUp),
                'd' => Some(ScrollAction::HalfPageDown),
                'u' => Some(ScrollAction::HalfPageUp),
                'f' => Some(ScrollAction::PageDown),
                'b' => Some(ScrollAction::PageUp),
                _ => None,
            };
        }
        if self.pending_g {
            self.pending_g = false;
            return (ch == 'g').then_some(ScrollAction::JumpTop);
        }
        match ch {
            'j' => Some(ScrollAction::LineDown),
            'k' => Some(ScrollAction::LineUp),
            'G' => Some(ScrollAction::JumpBottom),
            'g' => {
                self.pending_g = true;
                None
            }
            'i' | 'q' => Some(ScrollAction::ExitScroll),
            _ => None,
        }
    }

    pub fn reset(&mut self) {
        self.pending_g = false;
    }
}

/// Apply one scroll action given the transcript page height in visible lines.
/// Returns `true` when the action asks to leave scroll mode. Half pages round
/// down but never below one line, so tiny terminals still move.
pub fn apply_scroll_action(
    scroll: &mut TranscriptScroll,
    action: ScrollAction,
    page: usize,
) -> bool {
    let half = (page / 2).max(1);
    match action {
        ScrollAction::LineUp => scroll.line_up(),
        ScrollAction::LineDown => scroll.line_down(),
        ScrollAction::HalfPageUp => scroll.page_up(half),
        ScrollAction::HalfPageDown => scroll.page_down(half),
        ScrollAction::PageUp => scroll.page_up(page.max(1)),
        ScrollAction::PageDown => scroll.page_down(page.max(1)),
        ScrollAction::JumpTop => scroll.jump_to_start(),
        ScrollAction::JumpBottom => scroll.jump_to_end(),
        ScrollAction::ExitScroll => return true,
    }
    false
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptScroll {
    from_bottom: usize,
}

impl TranscriptScroll {
    pub fn line_up(&mut self) {
        self.from_bottom = self.from_bottom.saturating_add(1);
    }

    pub fn line_down(&mut self) {
        self.from_bottom = self.from_bottom.saturating_sub(1);
    }

    pub fn page_up(&mut self, lines: usize) {
        self.from_bottom = self.from_bottom.saturating_add(lines);
    }

    pub fn page_down(&mut self, lines: usize) {
        self.from_bottom = self.from_bottom.saturating_sub(lines);
    }

    pub fn jump_to_start(&mut self) {
        self.from_bottom = usize::MAX;
    }

    pub fn jump_to_end(&mut self) {
        self.from_bottom = 0;
    }

    pub fn bottom_offset(&self) -> usize {
        self.from_bottom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_inserts_and_deletes_at_the_cursor() {
        let mut composer = Composer::default();
        composer.insert_str("helo world");
        for _ in 0..7 {
            composer.move_left();
        }
        composer.insert_char('l');
        assert_eq!(composer.as_str(), "hello world");
        composer.move_right();
        composer.delete();
        assert_eq!(composer.as_str(), "helloworld");
        composer.backspace();
        assert_eq!(composer.as_str(), "hellworld");
    }

    #[test]
    fn composer_navigation_and_deletion_preserve_graphemes() {
        let mut composer = Composer::default();
        composer.insert_str("a👩‍🔬e\u{301}z");
        composer.move_left();
        composer.move_left();
        let before_combining_grapheme = composer.cursor();
        composer.move_left();
        assert!(composer.cursor() < before_combining_grapheme);
        composer.delete();
        assert_eq!(composer.as_str(), "ae\u{301}z");
        composer.move_end();
        composer.backspace();
        assert_eq!(composer.as_str(), "ae\u{301}");
        composer.backspace();
        assert_eq!(composer.as_str(), "a");
    }

    #[test]
    fn composer_home_end_replace_and_take_keep_cursor_valid() {
        let mut composer = Composer::default();
        composer.replace("middle".to_string());
        assert_eq!(composer.cursor(), "middle".len());
        composer.move_home();
        composer.insert_str("start-");
        composer.move_end();
        composer.insert_str("-end");
        assert_eq!(composer.take(), "start-middle-end");
        assert_eq!(composer.as_str(), "");
        assert_eq!(composer.cursor(), 0);
    }

    #[test]
    fn composer_moves_vertically_and_preserves_preferred_column() {
        let mut composer = Composer::default();
        composer.replace("abcd\nx\nwxyz".to_string());
        assert!(composer.move_up());
        assert_eq!(&composer.as_str()[..composer.cursor()], "abcd\nx");
        assert!(composer.move_up());
        assert_eq!(&composer.as_str()[..composer.cursor()], "abcd");
        assert!(!composer.move_up());
        assert!(composer.move_down());
        assert_eq!(&composer.as_str()[..composer.cursor()], "abcd\nx");
        assert!(composer.move_down());
        assert_eq!(composer.cursor(), composer.as_str().len());
        assert!(!composer.move_down());
    }

    #[test]
    fn composer_vertical_motion_uses_display_columns() {
        let mut composer = Composer::default();
        composer.replace("界x\nabcd".to_string());
        composer.move_home();
        composer.move_right();
        assert!(composer.move_down());
        assert_eq!(&composer.as_str()[..composer.cursor()], "界x\nab");
    }

    #[test]
    fn vertical_motion_snaps_wide_cells_and_keeps_long_preferred_column() {
        let mut composer = Composer::default();
        composer.replace("a\n界".to_string());
        composer.move_home();
        composer.move_right();
        assert!(composer.move_down());
        assert_eq!(&composer.as_str()[composer.cursor()..], "界");
        assert!(composer.as_str().is_char_boundary(composer.cursor()));

        composer.replace("abcdef\nx\n界\nabc".to_string());
        composer.move_home();
        for _ in 0..6 {
            composer.move_right();
        }
        for expected_prefix in ["abcdef\nx", "abcdef\nx\n界", "abcdef\nx\n界\nabc"] {
            assert!(composer.move_down());
            assert_eq!(&composer.as_str()[..composer.cursor()], expected_prefix);
            assert!(composer.as_str().is_char_boundary(composer.cursor()));
        }
    }

    #[test]
    fn history_is_bounded_and_restores_the_draft() {
        let mut history = ComposerHistory::new(2);
        history.record("one");
        history.record("two");
        history.record("three");

        assert_eq!(history.len(), 2);
        assert_eq!(history.previous("draft"), Some("three".to_string()));
        assert_eq!(history.previous("three"), Some("two".to_string()));
        assert_eq!(history.previous("two"), Some("two".to_string()));
        assert_eq!(history.next(), Some("three".to_string()));
        assert_eq!(history.next(), Some("draft".to_string()));
        assert_eq!(history.next(), None);
    }

    #[test]
    fn recalled_history_places_cursor_at_end_and_remains_editable() {
        let mut history = ComposerHistory::new(2);
        history.record("hello");
        let mut composer = Composer::default();
        composer.replace(history.previous(composer.as_str()).expect("history entry"));
        assert_eq!(composer.cursor(), composer.as_str().len());
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.as_str(), "helo");
    }

    #[test]
    fn vim_keys_map_line_half_and_full_page_motions() {
        let mut keys = VimScrollKeys::default();
        assert_eq!(keys.interpret('j', false), Some(ScrollAction::LineDown));
        assert_eq!(keys.interpret('k', false), Some(ScrollAction::LineUp));
        assert_eq!(keys.interpret('e', true), Some(ScrollAction::LineDown));
        assert_eq!(keys.interpret('y', true), Some(ScrollAction::LineUp));
        assert_eq!(keys.interpret('d', true), Some(ScrollAction::HalfPageDown));
        assert_eq!(keys.interpret('u', true), Some(ScrollAction::HalfPageUp));
        assert_eq!(keys.interpret('f', true), Some(ScrollAction::PageDown));
        assert_eq!(keys.interpret('b', true), Some(ScrollAction::PageUp));
        assert_eq!(keys.interpret('G', false), Some(ScrollAction::JumpBottom));
        assert_eq!(keys.interpret('i', false), Some(ScrollAction::ExitScroll));
        assert_eq!(keys.interpret('q', false), Some(ScrollAction::ExitScroll));
        assert_eq!(keys.interpret('x', false), None);
    }

    #[test]
    fn gg_is_a_chord_and_non_g_cancels_it() {
        let mut keys = VimScrollKeys::default();
        assert_eq!(keys.interpret('g', false), None);
        assert_eq!(keys.interpret('g', false), Some(ScrollAction::JumpTop));
        // g then j: the chord is cancelled and the j is swallowed, never
        // reinterpreted as its own motion.
        assert_eq!(keys.interpret('g', false), None);
        assert_eq!(keys.interpret('j', false), None);
        assert_eq!(keys.interpret('j', false), Some(ScrollAction::LineDown));
        // Ctrl keys and reset() both clear a pending chord.
        assert_eq!(keys.interpret('g', false), None);
        assert_eq!(keys.interpret('d', true), Some(ScrollAction::HalfPageDown));
        assert_eq!(keys.interpret('g', false), None);
        keys.reset();
        assert_eq!(keys.interpret('g', false), None);
    }

    #[test]
    fn apply_scroll_action_moves_by_line_half_and_full_pages() {
        let mut scroll = TranscriptScroll::default();
        assert!(!apply_scroll_action(&mut scroll, ScrollAction::PageUp, 10));
        assert_eq!(scroll.bottom_offset(), 10);
        assert!(!apply_scroll_action(
            &mut scroll,
            ScrollAction::HalfPageDown,
            10
        ));
        assert_eq!(scroll.bottom_offset(), 5);
        assert!(!apply_scroll_action(&mut scroll, ScrollAction::LineUp, 10));
        assert_eq!(scroll.bottom_offset(), 6);
        assert!(!apply_scroll_action(
            &mut scroll,
            ScrollAction::LineDown,
            10
        ));
        assert_eq!(scroll.bottom_offset(), 5);
        assert!(!apply_scroll_action(&mut scroll, ScrollAction::JumpTop, 10));
        assert_eq!(scroll.bottom_offset(), usize::MAX);
        assert!(!apply_scroll_action(
            &mut scroll,
            ScrollAction::JumpBottom,
            10
        ));
        assert_eq!(scroll.bottom_offset(), 0);
        // Degenerate one-line viewport: half a page still moves one line.
        assert!(!apply_scroll_action(
            &mut scroll,
            ScrollAction::HalfPageUp,
            1
        ));
        assert_eq!(scroll.bottom_offset(), 1);
        // ExitScroll mutates nothing and reports the mode change.
        assert!(apply_scroll_action(
            &mut scroll,
            ScrollAction::ExitScroll,
            10
        ));
        assert_eq!(scroll.bottom_offset(), 1);
    }

    #[test]
    fn scroll_navigation_is_saturating() {
        let mut scroll = TranscriptScroll::default();
        scroll.page_up(10);
        scroll.page_up(5);
        assert_eq!(scroll.bottom_offset(), 15);
        scroll.page_down(4);
        assert_eq!(scroll.bottom_offset(), 11);
        scroll.jump_to_end();
        assert_eq!(scroll.bottom_offset(), 0);
        scroll.jump_to_start();
        assert_eq!(scroll.bottom_offset(), usize::MAX);
    }
}
