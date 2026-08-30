//! Canonical ordered session timeline reducer (Vikunja #1338).

use crate::session_protocol::{
    ActiveToolState, HistoryWindow, SessionEvent, TimelineEntry, TimelineEntryKind,
    ToolCallStateStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineReducer {
    timeline: Vec<TimelineEntry>,
    active_tools: Vec<ActiveToolState>,
    history_window: HistoryWindow,
    max_entries: usize,
    next_id: u64,
    open_entry_id: Option<u64>,
}

impl TimelineReducer {
    pub fn new(max_entries: usize) -> Self {
        Self::from_parts(
            Vec::new(),
            Vec::new(),
            HistoryWindow::complete(0),
            max_entries,
        )
    }

    pub fn from_parts(
        timeline: Vec<TimelineEntry>,
        active_tools: Vec<ActiveToolState>,
        history_window: HistoryWindow,
        max_entries: usize,
    ) -> Self {
        let next_id = timeline
            .iter()
            .map(|entry| entry.id.max(entry.order))
            .chain(active_tools.iter().map(|tool| tool.occurrence_id))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut reducer = Self {
            timeline,
            active_tools,
            history_window,
            max_entries: max_entries.max(1),
            next_id,
            open_entry_id: None,
        };
        for tool in reducer.active_tools.clone() {
            reducer.update_occurrence(tool.occurrence_id, tool.status, None);
        }
        reducer.trim_to_limit();
        reducer.sync_window();
        reducer
    }

    pub fn timeline(&self) -> &[TimelineEntry] {
        &self.timeline
    }

    pub fn active_tools(&self) -> &[ActiveToolState] {
        &self.active_tools
    }

    pub fn history_window(&self) -> &HistoryWindow {
        &self.history_window
    }

    pub fn is_open(&self, id: u64) -> bool {
        self.open_entry_id == Some(id)
    }

    pub fn into_parts(self) -> (Vec<TimelineEntry>, Vec<ActiveToolState>, HistoryWindow) {
        (self.timeline, self.active_tools, self.history_window)
    }

    pub fn apply(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::UserMessage { text, .. } => {
                self.close_open();
                self.push(TimelineEntryKind::User {
                    text,
                    content_truncated: false,
                });
            }
            SessionEvent::AssistantDelta { text } => {
                self.append_text(TextKind::Assistant, text);
            }
            SessionEvent::ThoughtDelta { text } => {
                self.append_text(TextKind::Thought, text);
            }
            SessionEvent::AssistantDone { outcome } => {
                self.close_open();
                self.push(TimelineEntryKind::Outcome { outcome });
            }
            SessionEvent::ToolCallStarted {
                id, name, title, ..
            } => {
                self.close_open();
                let occurrence_id = self.push(TimelineEntryKind::Tool {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    title: title.clone(),
                    status: ToolCallStateStatus::Pending,
                    output: None,
                    content_truncated: false,
                });
                self.active_tools.push(ActiveToolState {
                    occurrence_id,
                    tool_call_id: id,
                    name,
                    title,
                    status: ToolCallStateStatus::Pending,
                    content_truncated: false,
                });
            }
            SessionEvent::ToolCallUpdated { id, status } => {
                self.update_tool(&id, status, None);
            }
            SessionEvent::ToolCallFinished { id, status, output } => {
                self.update_tool(&id, status, Some(output));
            }
            SessionEvent::ConversationCleared => self.clear(),
            SessionEvent::ApprovalRequested { .. }
            | SessionEvent::ApprovalResolved { .. }
            | SessionEvent::ApprovalDeadlineChanged { .. }
            | SessionEvent::RuntimeOptionsChanged { .. }
            | SessionEvent::ContextUsageChanged { .. }
            | SessionEvent::TurnStatusChanged { .. }
            | SessionEvent::SessionEnding { .. } => {}
        }
        self.sync_window();
    }

    pub fn push_reconstructed(&mut self, entry: TimelineEntryKind) -> u64 {
        self.close_open();
        self.push(entry)
    }

    pub fn start_reconstructed_tool(
        &mut self,
        tool_call_id: String,
        name: String,
        title: String,
    ) -> u64 {
        self.close_open();
        let occurrence_id = self.push(TimelineEntryKind::Tool {
            tool_call_id: tool_call_id.clone(),
            name: name.clone(),
            title: title.clone(),
            status: ToolCallStateStatus::InProgress,
            output: None,
            content_truncated: false,
        });
        self.active_tools.push(ActiveToolState {
            occurrence_id,
            tool_call_id,
            name,
            title,
            status: ToolCallStateStatus::InProgress,
            content_truncated: false,
        });
        occurrence_id
    }

    pub fn push_system(&mut self, text: String) {
        self.close_open();
        self.push(TimelineEntryKind::System {
            text,
            content_truncated: false,
        });
    }

    pub fn update_reconstructed_tool(
        &mut self,
        tool_call_id: &str,
        status: ToolCallStateStatus,
        output: String,
    ) {
        self.update_tool(tool_call_id, status, Some(output));
    }

    pub fn cancel_reconstructed_active_tools(&mut self) {
        let active = std::mem::take(&mut self.active_tools);
        for tool in active {
            self.update_occurrence(tool.occurrence_id, ToolCallStateStatus::Cancelled, None);
        }
    }

    pub fn clear(&mut self) {
        self.timeline.clear();
        self.active_tools.clear();
        self.history_window = HistoryWindow::complete(0);
        self.open_entry_id = None;
    }

    fn append_text(&mut self, kind: TextKind, text: String) {
        if let Some(open_id) = self.open_entry_id {
            if let Some(last) = self.timeline.last_mut() {
                if last.id == open_id {
                    let target = match (&mut last.entry, kind) {
                        (TimelineEntryKind::Assistant { text, .. }, TextKind::Assistant)
                        | (TimelineEntryKind::Thought { text, .. }, TextKind::Thought) => {
                            Some(text)
                        }
                        _ => None,
                    };
                    if let Some(target) = target {
                        target.push_str(&text);
                        return;
                    }
                }
            }
        }
        self.close_open();
        let entry = match kind {
            TextKind::Assistant => TimelineEntryKind::Assistant {
                text,
                content_truncated: false,
            },
            TextKind::Thought => TimelineEntryKind::Thought {
                text,
                content_truncated: false,
            },
        };
        self.open_entry_id = Some(self.push(entry));
    }

    fn update_tool(
        &mut self,
        tool_call_id: &str,
        status: ToolCallStateStatus,
        output: Option<String>,
    ) {
        let occurrence_id = self
            .timeline
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                TimelineEntryKind::Tool {
                    tool_call_id: candidate,
                    status,
                    ..
                } if candidate == tool_call_id && !status.is_terminal() => Some(entry.id),
                _ => None,
            })
            .or_else(|| {
                self.active_tools
                    .iter()
                    .rev()
                    .find(|tool| tool.tool_call_id == tool_call_id)
                    .map(|tool| tool.occurrence_id)
            });
        if let Some(occurrence_id) = occurrence_id {
            self.update_occurrence(occurrence_id, status, output);
            if status.is_terminal() {
                self.active_tools
                    .retain(|tool| tool.occurrence_id != occurrence_id);
            } else if let Some(tool) = self
                .active_tools
                .iter_mut()
                .find(|tool| tool.occurrence_id == occurrence_id)
            {
                tool.status = status;
            }
        }
    }

    fn update_occurrence(
        &mut self,
        occurrence_id: u64,
        status: ToolCallStateStatus,
        output: Option<String>,
    ) {
        if let Some(entry) = self.timeline.iter_mut().rev().find(|entry| {
            entry.id == occurrence_id && matches!(entry.entry, TimelineEntryKind::Tool { .. })
        }) {
            if let TimelineEntryKind::Tool {
                status: current,
                output: current_output,
                ..
            } = &mut entry.entry
            {
                *current = status;
                if let Some(output) = output {
                    *current_output = Some(output);
                }
            }
        }
    }

    fn push(&mut self, entry: TimelineEntryKind) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.timeline.push(TimelineEntry {
            id,
            order: id,
            entry,
        });
        self.trim_to_limit();
        self.sync_window();
        id
    }

    fn close_open(&mut self) {
        self.open_entry_id = None;
    }

    fn trim_to_limit(&mut self) {
        let excess = self.timeline.len().saturating_sub(self.max_entries);
        if excess > 0 {
            self.timeline.drain(..excess);
            self.history_window.truncated_before = self
                .history_window
                .truncated_before
                .saturating_add(excess as u64);
            if self
                .open_entry_id
                .is_some_and(|id| !self.timeline.iter().any(|entry| entry.id == id))
            {
                self.open_entry_id = None;
            }
        }
    }

    fn sync_window(&mut self) {
        self.history_window.retained = self.timeline.len();
        self.history_window.total = Some(
            self.history_window
                .truncated_before
                .saturating_add(self.timeline.len() as u64),
        );
    }
}

#[derive(Clone, Copy)]
enum TextKind {
    Assistant,
    Thought,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_protocol::AssistantOutcome;

    fn tool_status(entry: &TimelineEntry) -> Option<ToolCallStateStatus> {
        match entry.entry {
            TimelineEntryKind::Tool { status, .. } => Some(status),
            _ => None,
        }
    }

    #[test]
    fn tool_start_closes_text_and_preserves_interleaving() {
        let mut reducer = TimelineReducer::new(32);
        reducer.apply(SessionEvent::AssistantDelta {
            text: "before".into(),
        });
        reducer.apply(SessionEvent::ToolCallStarted {
            id: "call".into(),
            name: "read".into(),
            title: "read".into(),
            input_summary: None,
        });
        reducer.apply(SessionEvent::AssistantDelta {
            text: "after".into(),
        });

        assert!(matches!(
            reducer.timeline()[0].entry,
            TimelineEntryKind::Assistant { .. }
        ));
        assert!(matches!(
            reducer.timeline()[1].entry,
            TimelineEntryKind::Tool { .. }
        ));
        assert!(matches!(
            reducer.timeline()[2].entry,
            TimelineEntryKind::Assistant { .. }
        ));
    }

    #[test]
    fn reused_provider_id_updates_newest_nonterminal_occurrence() {
        let mut reducer = TimelineReducer::new(32);
        for title in ["first", "second"] {
            reducer.apply(SessionEvent::ToolCallStarted {
                id: "call_0".into(),
                name: "exec".into(),
                title: title.into(),
                input_summary: None,
            });
        }

        assert_ne!(reducer.timeline()[0].id, reducer.timeline()[1].id);
        assert_eq!(reducer.active_tools().len(), 2);
        reducer.apply(SessionEvent::ToolCallFinished {
            id: "call_0".into(),
            status: ToolCallStateStatus::Completed,
            output: "second output".into(),
        });
        assert_eq!(
            reducer
                .timeline()
                .iter()
                .filter_map(tool_status)
                .collect::<Vec<_>>(),
            vec![ToolCallStateStatus::Pending, ToolCallStateStatus::Completed]
        );
        assert_eq!(reducer.active_tools().len(), 1);

        reducer.apply(SessionEvent::ToolCallFinished {
            id: "call_0".into(),
            status: ToolCallStateStatus::Completed,
            output: "first output".into(),
        });
        assert_eq!(
            reducer
                .timeline()
                .iter()
                .filter_map(tool_status)
                .collect::<Vec<_>>(),
            vec![
                ToolCallStateStatus::Completed,
                ToolCallStateStatus::Completed
            ]
        );
    }

    #[test]
    fn active_tools_survive_history_trimming_and_remain_authoritative() {
        let mut reducer = TimelineReducer::new(1);
        reducer.apply(SessionEvent::ToolCallStarted {
            id: "long".into(),
            name: "exec".into(),
            title: "long".into(),
            input_summary: None,
        });
        reducer.apply(SessionEvent::UserMessage {
            text: "new".into(),
            request_id: None,
        });

        assert_eq!(reducer.timeline().len(), 1);
        assert_eq!(reducer.active_tools()[0].tool_call_id, "long");
        assert_eq!(reducer.history_window().truncated_before, 1);

        reducer.apply(SessionEvent::ToolCallFinished {
            id: "long".into(),
            status: ToolCallStateStatus::Completed,
            output: "done".into(),
        });
        assert!(reducer.active_tools().is_empty());
    }

    #[test]
    fn outcome_is_its_own_ordered_entry() {
        let mut reducer = TimelineReducer::new(32);
        reducer.apply(SessionEvent::AssistantDelta {
            text: "answer".into(),
        });
        reducer.apply(SessionEvent::AssistantDone {
            outcome: AssistantOutcome::Completed,
        });
        assert!(matches!(
            reducer.timeline()[1].entry,
            TimelineEntryKind::Outcome {
                outcome: AssistantOutcome::Completed
            }
        ));
    }
}
