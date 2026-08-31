//! Pure frontend state reducer shared by headless, TUI, and contract clients.
#![allow(dead_code)] // Renderers and clients consume subsets incrementally.
//!
//! [`ViewState`] is a deterministic fold over the canonical session stream. It
//! consumes:
//!
//! * `(seq, SessionEvent)` deltas via [`ViewState::apply_event`], and
//! * canonical [`SessionSnapshot`]s via [`ViewState::apply_snapshot`]
//!   (used on attach and on reconnect/lag recovery).
//!
//! The reducer owns *canonical ordering only*: it applies an event iff its
//! sequence number is exactly one past the last applied one. Stale/duplicate
//! sequences are ignored; a forward gap is reported so the client task can ask
//! the daemon for a fresh snapshot (`SyncRequest` in `session_protocol`) rather
//! than render a torn transcript. This mirrors the reconnect contract in
//! ADR-010 and keeps every rendering layer a pure function of `ViewState`.

use crate::session_protocol::{
    ActiveToolState, ApprovalRequest, AssistantOutcome, ContextUsage, DurabilityStatus,
    HistoryWindow, RuntimeOption, SessionEvent, SessionSnapshot, TimelineEntry, TimelineEntryKind,
    ToolCallState, TranscriptRole, TurnStatus,
};
use crate::session_timeline::TimelineReducer;

/// One rendered line of the conversation transcript.
///
/// Assistant and thought lines are accumulated incrementally from streamed
/// deltas; `open == true` marks the trailing line that is still receiving
/// deltas (a renderer may show a cursor/spinner on it). User and system lines
/// are always committed (`open == false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewLine {
    pub role: TranscriptRole,
    pub text: String,
    pub open: bool,
}

impl ViewLine {
    fn committed(role: TranscriptRole, text: String) -> Self {
        Self {
            role,
            text,
            open: false,
        }
    }
}

/// Result of feeding one sequenced event into the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The event was in order and mutated the state.
    Applied,
    /// The event's sequence was already seen (`seq <= last_seq`); ignored.
    Duplicate,
    /// A forward gap: the event's sequence is beyond the next expected one, so
    /// it was **not** applied. The client should request a fresh snapshot
    /// starting from `expected_seq`.
    Gap { expected_seq: u64 },
}

/// Render-ready projection of a single agent session.
///
/// Cheap to clone-compare in tests; holds no async or terminal handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewState {
    session_id: String,
    /// Highest sequence number applied so far. `0` means "nothing applied yet"
    /// (sequence numbers from the router are 1-based).
    last_seq: u64,
    turn_status: TurnStatus,
    durability_status: DurabilityStatus,
    timeline: TimelineReducer,
    // Temporary compatibility projections for callers not yet timeline-aware.
    transcript: Vec<ViewLine>,
    tool_calls: Vec<ToolCallState>,
    pending_approvals: Vec<ApprovalRequest>,
    runtime_options: Vec<RuntimeOption>,
    context_usage: Option<ContextUsage>,
    max_scrollback_entries: usize,
    /// Set once a `SessionEnding` event arrives; a renderer can surface this
    /// and stop accepting input.
    ending_reason: Option<String>,
}

impl ViewState {
    /// A fresh, empty view for `session_id` before any snapshot/event.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_scrollback_limit(session_id, crate::config::DEFAULT_TUI_SCROLLBACK_ENTRIES)
    }

    pub fn with_scrollback_limit(
        session_id: impl Into<String>,
        max_scrollback_entries: usize,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            last_seq: 0,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Saved,
            timeline: TimelineReducer::new(max_scrollback_entries),
            transcript: Vec::new(),
            tool_calls: Vec::new(),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
            max_scrollback_entries: max_scrollback_entries.max(1),
            ending_reason: None,
        }
    }

    // ---- read accessors (rendering layer consumes these) -----------------

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }
    pub fn turn_status(&self) -> TurnStatus {
        self.turn_status
    }
    pub fn durability_status(&self) -> DurabilityStatus {
        self.durability_status
    }
    pub fn timeline(&self) -> &[TimelineEntry] {
        self.timeline.timeline()
    }
    pub fn active_tools(&self) -> &[ActiveToolState] {
        self.timeline.active_tools()
    }
    pub fn history_window(&self) -> &HistoryWindow {
        self.timeline.history_window()
    }
    pub fn transcript(&self) -> &[ViewLine] {
        &self.transcript
    }
    pub fn tool_calls(&self) -> &[ToolCallState] {
        &self.tool_calls
    }
    pub fn pending_approvals(&self) -> &[ApprovalRequest] {
        &self.pending_approvals
    }
    pub fn runtime_options(&self) -> &[RuntimeOption] {
        &self.runtime_options
    }
    pub fn context_usage(&self) -> Option<&ContextUsage> {
        self.context_usage.as_ref()
    }
    pub fn history_truncated(&self) -> bool {
        self.timeline.history_window().is_truncated()
    }
    pub fn ending_reason(&self) -> Option<&str> {
        self.ending_reason.as_deref()
    }
    /// The single pending approval a modal should show, if any (the oldest).
    pub fn active_approval(&self) -> Option<&ApprovalRequest> {
        self.pending_approvals.first()
    }

    /// Clear local conversation/tool presentation after `/clear`.
    ///
    /// Session identity, runtime options, usage, and sequence ordering remain
    /// intact; the backing [`AgentSession`] clears its model history separately.
    pub fn clear_transcript(&mut self) {
        self.timeline.clear();
        self.sync_projection();
    }

    /// Append a committed frontend-local notice without changing session
    /// sequencing. Slash-command help and validation use this for information
    /// that is intentionally not sent to the model or daemon.
    pub fn push_system_message(&mut self, text: impl Into<String>) {
        self.timeline.push_system(text.into());
        self.sync_projection();
    }

    // ---- snapshot application (attach / reconnect) -----------------------

    /// Establish identity/sequence for an attached client that cannot observe
    /// snapshots. Any stale projection from a prior attachment is cleared.
    pub fn apply_attach_watermark(&mut self, session_id: String, seq: u64) {
        self.session_id = session_id;
        self.last_seq = seq;
        self.turn_status = TurnStatus::Idle;
        self.durability_status = DurabilityStatus::Saved;
        self.timeline.clear();
        self.transcript.clear();
        self.tool_calls.clear();
        self.pending_approvals.clear();
        self.runtime_options.clear();
        self.context_usage = None;
        self.ending_reason = None;
    }

    /// Replace the entire view with a canonical daemon snapshot.
    ///
    /// Used on first attach and whenever the client recovers from a gap. The
    /// snapshot's `seq` becomes the new watermark, so subsequent in-order
    /// events resume cleanly from there.
    pub fn apply_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.session_id = snapshot.session_id;
        self.last_seq = snapshot.seq;
        self.turn_status = snapshot.turn_status;
        self.durability_status = snapshot.durability_status;
        self.timeline = TimelineReducer::from_parts(
            snapshot.timeline,
            snapshot.active_tools,
            snapshot.history_window,
            self.max_scrollback_entries,
        );
        self.pending_approvals = snapshot.pending_approvals;
        self.runtime_options = snapshot.runtime_options;
        self.context_usage = snapshot.context_usage;
        self.sync_projection();
        // A snapshot is a full canonical resync of live session state, so any
        // `ending_reason` observed before a reconnect must clear: if the
        // session were really ending, that would arrive again as a fresh
        // `SessionEnding` event after the snapshot. Leaving it set would wedge
        // renderers that gate input on `ending_reason().is_some()`.
        self.ending_reason = None;
    }

    // ---- event application ----------------------------------------------

    /// Fold one sequenced event into the view.
    ///
    /// Returns [`ApplyOutcome`] describing whether the event was applied,
    /// ignored as a duplicate, or skipped because of a forward gap.
    pub fn apply_event(&mut self, seq: u64, event: SessionEvent) -> ApplyOutcome {
        if seq <= self.last_seq {
            return ApplyOutcome::Duplicate;
        }
        let expected = self.last_seq + 1;
        if seq != expected {
            return ApplyOutcome::Gap {
                expected_seq: expected,
            };
        }
        self.last_seq = seq;
        self.timeline.apply(event.clone());
        self.apply_event_body(event);
        self.sync_projection();
        ApplyOutcome::Applied
    }

    fn apply_event_body(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::UserMessage { .. }
            | SessionEvent::AssistantDelta { .. }
            | SessionEvent::ThoughtDelta { .. }
            | SessionEvent::AssistantDone { .. }
            | SessionEvent::ToolCallStarted { .. }
            | SessionEvent::ToolCallUpdated { .. }
            | SessionEvent::ToolCallFinished { .. } => {}
            SessionEvent::ApprovalRequested { request } => {
                if !self.pending_approvals.iter().any(|a| a.id == request.id) {
                    self.pending_approvals.push(request);
                }
            }
            SessionEvent::ApprovalResolved { approval_id, .. } => {
                self.pending_approvals.retain(|a| a.id != approval_id);
            }
            SessionEvent::ApprovalDeadlineChanged {
                approval_id,
                ineligible_deadline_unix_ms,
                paused,
            } => {
                if let Some(approval) = self
                    .pending_approvals
                    .iter_mut()
                    .find(|approval| approval.id == approval_id)
                {
                    approval.ineligible_deadline_unix_ms = Some(ineligible_deadline_unix_ms);
                    approval.deadline_paused = paused;
                }
            }
            SessionEvent::RuntimeOptionsChanged { options } => {
                self.runtime_options = options;
            }
            SessionEvent::ContextUsageChanged { usage } => {
                self.context_usage = Some(usage);
            }
            SessionEvent::ConversationCleared => {
                self.pending_approvals.clear();
                self.ending_reason = None;
            }
            SessionEvent::TurnStatusChanged { status } => {
                self.turn_status = status;
            }
            SessionEvent::DurabilityStatusChanged { status } => {
                self.durability_status = status;
            }
            SessionEvent::SessionEnding { reason } => {
                self.ending_reason = Some(reason);
            }
        }
    }

    fn sync_projection(&mut self) {
        self.transcript = self
            .timeline
            .timeline()
            .iter()
            .filter_map(|entry| match &entry.entry {
                TimelineEntryKind::User { text, .. } => {
                    Some(ViewLine::committed(TranscriptRole::User, text.clone()))
                }
                TimelineEntryKind::Assistant { text, .. } => Some(ViewLine {
                    role: TranscriptRole::Assistant,
                    text: text.clone(),
                    open: self.timeline.is_open(entry.id),
                }),
                TimelineEntryKind::Thought { text, .. } => Some(ViewLine {
                    role: TranscriptRole::Thought,
                    text: text.clone(),
                    open: self.timeline.is_open(entry.id),
                }),
                TimelineEntryKind::System { text, .. } => {
                    Some(ViewLine::committed(TranscriptRole::System, text.clone()))
                }
                TimelineEntryKind::Outcome { outcome } => outcome_note(outcome)
                    .map(|note| ViewLine::committed(TranscriptRole::System, note)),
                TimelineEntryKind::Tool { .. } => None,
            })
            .collect();
        self.tool_calls = self
            .timeline
            .timeline()
            .iter()
            .filter_map(|entry| match &entry.entry {
                TimelineEntryKind::Tool {
                    tool_call_id,
                    name,
                    title,
                    status,
                    output,
                    ..
                } => Some(ToolCallState {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    title: title.clone(),
                    status: *status,
                    output: output.clone(),
                }),
                _ => None,
            })
            .collect();
    }
}

/// A privacy-safe system note appended to the transcript when a turn ends in a
/// non-clean outcome. `Completed` returns `None` (nothing to annotate).
fn outcome_note(outcome: &AssistantOutcome) -> Option<String> {
    match outcome {
        AssistantOutcome::Completed => None,
        AssistantOutcome::Errored { message, .. } => Some(format!("[turn errored: {message}]")),
        AssistantOutcome::Refused => Some("[turn refused]".to_string()),
        AssistantOutcome::Aborted => Some("[turn interrupted]".to_string()),
        AssistantOutcome::MaxTokens => Some("[turn hit the output token limit]".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_protocol::{ContextUsage, ToolCallStateStatus};

    fn ev(state: &mut ViewState, seq: u64, event: SessionEvent) -> ApplyOutcome {
        state.apply_event(seq, event)
    }

    fn assistant_text(state: &ViewState) -> String {
        state
            .transcript()
            .iter()
            .filter(|l| l.role == TranscriptRole::Assistant)
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn assembles_user_and_streamed_assistant_turn() {
        let mut s = ViewState::new("sess-1");
        assert_eq!(
            ev(
                &mut s,
                1,
                SessionEvent::UserMessage {
                    text: "hi".into(),
                    request_id: None,
                },
            ),
            ApplyOutcome::Applied
        );
        ev(
            &mut s,
            2,
            SessionEvent::AssistantDelta { text: "Hel".into() },
        );
        ev(
            &mut s,
            3,
            SessionEvent::AssistantDelta { text: "lo!".into() },
        );
        // Mid-stream the assistant line is still open.
        assert!(s.transcript().last().unwrap().open);
        ev(
            &mut s,
            4,
            SessionEvent::AssistantDone {
                outcome: AssistantOutcome::Completed,
            },
        );
        assert_eq!(assistant_text(&s), "Hello!");
        assert!(!s.transcript().last().unwrap().open);
        assert_eq!(s.last_seq(), 4);
        // User line + one assistant line, no system note on clean completion.
        assert_eq!(s.transcript().len(), 2);
        assert_eq!(s.transcript()[0].role, TranscriptRole::User);
    }

    #[test]
    fn local_system_message_preserves_sequence_and_closes_stream() {
        let mut state = ViewState::new("sess-1");
        ev(
            &mut state,
            1,
            SessionEvent::AssistantDelta {
                text: "partial".into(),
            },
        );

        state.push_system_message("help");

        assert_eq!(state.last_seq(), 1);
        assert!(!state.transcript()[0].open);
        assert_eq!(state.transcript()[1].role, TranscriptRole::System);
        assert_eq!(state.transcript()[1].text, "help");
    }

    #[test]
    fn canonical_clear_resets_conversation_projection_at_its_sequence() {
        let mut state = ViewState::new("session-1");
        ev(
            &mut state,
            1,
            SessionEvent::UserMessage {
                text: "before".to_string(),
                request_id: None,
            },
        );
        ev(&mut state, 2, SessionEvent::ConversationCleared);
        assert!(state.transcript().is_empty());
        assert!(state.tool_calls().is_empty());
        assert!(state.pending_approvals().is_empty());
        assert_eq!(state.last_seq(), 2);

        ev(
            &mut state,
            3,
            SessionEvent::UserMessage {
                text: "after".to_string(),
                request_id: None,
            },
        );
        assert_eq!(state.transcript()[0].text, "after");
    }

    #[test]
    fn scrollback_limit_keeps_newest_ordered_timeline_entries() {
        let mut state = ViewState::with_scrollback_limit("sess-1", 2);
        for (seq, text) in [(1, "one"), (2, "two"), (3, "three")] {
            ev(
                &mut state,
                seq,
                SessionEvent::UserMessage {
                    text: text.into(),
                    request_id: None,
                },
            );
        }
        for (seq, id) in [(4, "t1"), (5, "t2"), (6, "t3")] {
            ev(
                &mut state,
                seq,
                SessionEvent::ToolCallStarted {
                    id: id.into(),
                    name: "read_file".into(),
                    title: id.into(),
                    input_summary: None,
                },
            );
        }

        assert!(state.transcript().is_empty());
        assert_eq!(state.tool_calls().len(), 2);
        assert_eq!(state.tool_calls()[0].id, "t2");
        assert_eq!(state.tool_calls()[1].id, "t3");
        assert_eq!(state.history_window().truncated_before, 4);
        assert_eq!(state.history_window().retained, 2);
        assert!(state.history_truncated());
    }

    #[test]
    fn canonical_clear_resets_accumulated_truncation() {
        let mut state = ViewState::with_scrollback_limit("sess-1", 1);
        for (seq, text) in [(1, "one"), (2, "two")] {
            ev(
                &mut state,
                seq,
                SessionEvent::UserMessage {
                    text: text.to_string(),
                    request_id: None,
                },
            );
        }
        assert!(state.history_truncated());
        ev(&mut state, 3, SessionEvent::ConversationCleared);
        assert!(!state.history_truncated());
    }

    #[test]
    fn duplicate_sequence_is_ignored() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::UserMessage {
                text: "a".into(),
                request_id: None,
            },
        );
        // Replayed frame.
        assert_eq!(
            ev(
                &mut s,
                1,
                SessionEvent::UserMessage {
                    text: "a".into(),
                    request_id: None,
                },
            ),
            ApplyOutcome::Duplicate
        );
        assert_eq!(s.transcript().len(), 1);
        assert_eq!(s.last_seq(), 1);
    }

    #[test]
    fn forward_gap_is_reported_and_not_applied() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::UserMessage {
                text: "a".into(),
                request_id: None,
            },
        );
        // seq 3 arrives before seq 2.
        assert_eq!(
            ev(&mut s, 3, SessionEvent::AssistantDelta { text: "x".into() }),
            ApplyOutcome::Gap { expected_seq: 2 }
        );
        // Nothing changed; watermark held at 1.
        assert_eq!(s.last_seq(), 1);
        assert_eq!(s.transcript().len(), 1);
        // The missing frame can still be applied in order.
        assert_eq!(
            ev(&mut s, 2, SessionEvent::AssistantDelta { text: "x".into() }),
            ApplyOutcome::Applied
        );
        assert_eq!(assistant_text(&s), "x");
    }

    #[test]
    fn thought_then_assistant_closes_the_thought_line() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::ThoughtDelta {
                text: "reasoning".into(),
            },
        );
        ev(
            &mut s,
            2,
            SessionEvent::AssistantDelta {
                text: "answer".into(),
            },
        );
        let roles: Vec<_> = s.transcript().iter().map(|l| l.role).collect();
        assert_eq!(
            roles,
            vec![TranscriptRole::Thought, TranscriptRole::Assistant]
        );
        // Thought line sealed, assistant line still streaming.
        assert!(!s.transcript()[0].open);
        assert!(s.transcript()[1].open);
    }

    #[test]
    fn tool_call_lifecycle() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::ToolCallStarted {
                id: "t1".into(),
                name: "read_file".into(),
                title: "Read file".into(),
                input_summary: Some("src/main.rs".into()),
            },
        );
        assert_eq!(s.tool_calls().len(), 1);
        assert_eq!(s.tool_calls()[0].status, ToolCallStateStatus::Pending);
        ev(
            &mut s,
            2,
            SessionEvent::ToolCallUpdated {
                id: "t1".into(),
                status: ToolCallStateStatus::InProgress,
            },
        );
        assert_eq!(s.tool_calls()[0].status, ToolCallStateStatus::InProgress);
        ev(
            &mut s,
            3,
            SessionEvent::ToolCallFinished {
                id: "t1".into(),
                status: ToolCallStateStatus::Completed,
                output: "done".into(),
            },
        );
        assert_eq!(s.tool_calls()[0].status, ToolCallStateStatus::Completed);
        assert_eq!(s.tool_calls()[0].output.as_deref(), Some("done"));
    }

    #[test]
    fn failed_tool_call_marks_failed() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::ToolCallStarted {
                id: "t1".into(),
                name: "exec".into(),
                title: "Run".into(),
                input_summary: None,
            },
        );
        ev(
            &mut s,
            2,
            SessionEvent::ToolCallFinished {
                id: "t1".into(),
                status: ToolCallStateStatus::Failed,
                output: "boom".into(),
            },
        );
        assert_eq!(s.tool_calls()[0].status, ToolCallStateStatus::Failed);
    }

    #[test]
    fn cancelled_tool_finish_remains_cancelled() {
        let mut state = ViewState::new("sess-1");
        ev(
            &mut state,
            1,
            SessionEvent::ToolCallStarted {
                id: "t1".into(),
                name: "exec".into(),
                title: "Run".into(),
                input_summary: None,
            },
        );
        ev(
            &mut state,
            2,
            SessionEvent::ToolCallFinished {
                id: "t1".into(),
                status: ToolCallStateStatus::Cancelled,
                output: "cancelled".into(),
            },
        );
        assert_eq!(state.tool_calls()[0].status, ToolCallStateStatus::Cancelled);
    }

    #[test]
    fn approval_requested_then_resolved() {
        let mut s = ViewState::new("sess-1");
        let req = ApprovalRequest {
            id: "ap1".into(),
            tool_call_id: "t1".into(),
            tool: "exec".into(),
            detail: "rm -rf".into(),
            allow_always_available: false,
            ineligible_deadline_unix_ms: None,
            deadline_paused: false,
        };
        ev(
            &mut s,
            1,
            SessionEvent::ApprovalRequested {
                request: req.clone(),
            },
        );
        assert_eq!(s.active_approval().map(|a| a.id.as_str()), Some("ap1"));
        // Duplicate request for the same id does not double-list.
        ev(&mut s, 2, SessionEvent::ApprovalRequested { request: req });
        assert_eq!(s.pending_approvals().len(), 1);
        ev(
            &mut s,
            3,
            SessionEvent::ApprovalDeadlineChanged {
                approval_id: "ap1".into(),
                ineligible_deadline_unix_ms: 123_456,
                paused: true,
            },
        );
        assert_eq!(
            s.active_approval().unwrap().ineligible_deadline_unix_ms,
            Some(123_456)
        );
        assert!(s.active_approval().unwrap().deadline_paused);
        ev(
            &mut s,
            4,
            SessionEvent::ApprovalResolved {
                approval_id: "ap1".into(),
                decision: crate::session_protocol::ApprovalDecision::AllowOnce,
                resolved_by: "local".into(),
            },
        );
        assert!(s.active_approval().is_none());
    }

    #[test]
    fn turn_status_context_and_ending_tracked() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            },
        );
        assert_eq!(s.turn_status(), TurnStatus::Running);
        ev(
            &mut s,
            2,
            SessionEvent::ContextUsageChanged {
                usage: ContextUsage::new(1000, Some(200_000), 4096, None),
            },
        );
        assert_eq!(s.context_usage().unwrap().prompt_tokens, 1000);
        ev(
            &mut s,
            3,
            SessionEvent::SessionEnding {
                reason: "stopped".into(),
            },
        );
        assert_eq!(s.ending_reason(), Some("stopped"));
    }

    #[test]
    fn errored_outcome_appends_system_note() {
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::AssistantDelta {
                text: "partial".into(),
            },
        );
        ev(
            &mut s,
            2,
            SessionEvent::AssistantDone {
                outcome: AssistantOutcome::Errored {
                    context_overflow: false,
                    message: "provider 500".into(),
                },
            },
        );
        let last = s.transcript().last().unwrap();
        assert_eq!(last.role, TranscriptRole::System);
        assert!(last.text.contains("provider 500"));
    }

    #[test]
    fn snapshot_resets_then_events_resume_in_order() {
        let mut s = ViewState::new("placeholder");
        // Some stale local state that the snapshot should blow away.
        ev(
            &mut s,
            1,
            SessionEvent::UserMessage {
                text: "stale".into(),
                request_id: None,
            },
        );

        let snap = SessionSnapshot {
            session_id: "sess-42".into(),
            seq: 10,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Saved,
            timeline: vec![
                TimelineEntry {
                    id: 1,
                    order: 1,
                    entry: TimelineEntryKind::User {
                        text: "question".into(),
                        content_truncated: false,
                    },
                },
                TimelineEntry {
                    id: 2,
                    order: 2,
                    entry: TimelineEntryKind::Assistant {
                        text: "answer".into(),
                        content_truncated: false,
                    },
                },
            ],
            active_tools: Vec::new(),
            history_window: HistoryWindow {
                truncated_before: 1,
                retained: 2,
                total: Some(3),
                continuation: None,
            },
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        };
        s.apply_snapshot(snap);
        assert_eq!(s.session_id(), "sess-42");
        assert_eq!(s.last_seq(), 10);
        assert_eq!(s.transcript().len(), 2);
        assert!(s.history_truncated());

        // A pre-snapshot sequence is now a duplicate.
        assert_eq!(
            ev(
                &mut s,
                5,
                SessionEvent::UserMessage {
                    text: "x".into(),
                    request_id: None,
                },
            ),
            ApplyOutcome::Duplicate
        );
        // The next in-order event (seq 11) applies on top of the snapshot.
        assert_eq!(
            ev(
                &mut s,
                11,
                SessionEvent::UserMessage {
                    text: "follow-up".into(),
                    request_id: None,
                }
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(s.transcript().len(), 3);
        assert_eq!(s.last_seq(), 11);
    }

    #[test]
    fn snapshot_hydration_uses_configured_scrollback_bound() {
        let mut state = ViewState::with_scrollback_limit("placeholder", 1);
        state.apply_snapshot(SessionSnapshot {
            session_id: "session".to_string(),
            seq: 2,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Saved,
            timeline: vec![
                TimelineEntry {
                    id: 1,
                    order: 1,
                    entry: TimelineEntryKind::User {
                        text: "old".to_string(),
                        content_truncated: false,
                    },
                },
                TimelineEntry {
                    id: 2,
                    order: 2,
                    entry: TimelineEntryKind::Assistant {
                        text: "new".to_string(),
                        content_truncated: false,
                    },
                },
            ],
            active_tools: Vec::new(),
            history_window: HistoryWindow::complete(2),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        });
        assert_eq!(state.transcript().len(), 1);
        assert_eq!(state.transcript()[0].text, "new");
        assert!(state.history_truncated());
    }

    #[test]
    fn snapshot_preserves_non_clean_terminal_outcome_note() {
        let outcome = AssistantOutcome::Errored {
            context_overflow: false,
            message: "provider unavailable".to_string(),
        };
        let mut live = ViewState::new("session");
        ev(
            &mut live,
            1,
            SessionEvent::AssistantDelta {
                text: "partial".to_string(),
            },
        );
        ev(
            &mut live,
            2,
            SessionEvent::AssistantDone {
                outcome: outcome.clone(),
            },
        );

        let mut restored = ViewState::new("session");
        restored.apply_snapshot(SessionSnapshot {
            session_id: "session".to_string(),
            seq: 2,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Saved,
            timeline: vec![
                TimelineEntry {
                    id: 1,
                    order: 1,
                    entry: TimelineEntryKind::Assistant {
                        text: "partial".to_string(),
                        content_truncated: false,
                    },
                },
                TimelineEntry {
                    id: 2,
                    order: 2,
                    entry: TimelineEntryKind::Outcome { outcome },
                },
            ],
            active_tools: Vec::new(),
            history_window: HistoryWindow::complete(2),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        });

        assert_eq!(restored.transcript(), live.transcript());
    }

    #[test]
    fn textless_outcomes_do_not_render_blank_assistant_rows() {
        let outcome = AssistantOutcome::Aborted;
        let mut live = ViewState::new("session");
        ev(
            &mut live,
            1,
            SessionEvent::AssistantDone {
                outcome: outcome.clone(),
            },
        );
        assert_eq!(live.transcript().len(), 1);
        assert_eq!(live.transcript()[0].role, TranscriptRole::System);

        let mut restored = ViewState::new("session");
        restored.apply_snapshot(SessionSnapshot {
            session_id: "session".to_string(),
            seq: 1,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Saved,
            timeline: vec![TimelineEntry {
                id: 1,
                order: 1,
                entry: TimelineEntryKind::Outcome { outcome },
            }],
            active_tools: Vec::new(),
            history_window: HistoryWindow::complete(1),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        });
        assert_eq!(restored.transcript(), live.transcript());
    }

    #[test]
    fn snapshot_clears_stale_ending_reason() {
        // A session that emitted SessionEnding, then reconnected and got a
        // fresh canonical snapshot, must not stay wedged as "ending".
        let mut s = ViewState::new("sess-1");
        ev(
            &mut s,
            1,
            SessionEvent::SessionEnding {
                reason: "idle timeout".into(),
            },
        );
        assert_eq!(s.ending_reason(), Some("idle timeout"));

        let snap = SessionSnapshot {
            session_id: "sess-1".into(),
            seq: 9,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Saved,
            timeline: Vec::new(),
            active_tools: Vec::new(),
            history_window: HistoryWindow::complete(0),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        };
        s.apply_snapshot(snap);
        assert_eq!(
            s.ending_reason(),
            None,
            "stale ending_reason survived resync"
        );
    }

    #[test]
    fn durability_status_applies_from_events_and_snapshot() {
        let mut state = ViewState::new("session");
        ev(
            &mut state,
            1,
            SessionEvent::DurabilityStatusChanged {
                status: DurabilityStatus::Unsaved,
            },
        );
        assert_eq!(state.durability_status(), DurabilityStatus::Unsaved);

        let mut snapshot = SessionSnapshot {
            session_id: "session".to_string(),
            seq: 2,
            turn_status: TurnStatus::Idle,
            durability_status: DurabilityStatus::Superseded,
            timeline: Vec::new(),
            active_tools: Vec::new(),
            history_window: HistoryWindow::complete(0),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        };
        state.apply_snapshot(snapshot.clone());
        assert_eq!(state.durability_status(), DurabilityStatus::Superseded);

        snapshot.durability_status = DurabilityStatus::Degraded;
        state.apply_snapshot(snapshot);
        assert_eq!(state.durability_status(), DurabilityStatus::Degraded);
    }
}
