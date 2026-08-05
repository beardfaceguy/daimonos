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

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptScroll {
    from_bottom: usize,
}

impl TranscriptScroll {
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
