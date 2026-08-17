//! Pure process-local input state for the interactive TUI.

use std::collections::VecDeque;

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
