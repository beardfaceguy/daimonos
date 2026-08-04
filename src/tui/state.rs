//! Pure TUI state reducer (Vikunja #1091, layer 2).
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
    ApprovalRequest, AssistantOutcome, ContextUsage, RuntimeOption, SessionEvent, SessionSnapshot,
    ToolCallState, ToolCallStateStatus, TranscriptEntry, TranscriptRole, TurnStatus,
};

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
    transcript: Vec<ViewLine>,
    tool_calls: Vec<ToolCallState>,
    pending_approvals: Vec<ApprovalRequest>,
    runtime_options: Vec<RuntimeOption>,
    context_usage: Option<ContextUsage>,
    /// Set once a `SessionEnding` event arrives; a renderer can surface this
    /// and stop accepting input.
    ending_reason: Option<String>,
}

impl ViewState {
    /// A fresh, empty view for `session_id` before any snapshot/event.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            last_seq: 0,
            turn_status: TurnStatus::Idle,
            transcript: Vec::new(),
            tool_calls: Vec::new(),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
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
    pub fn ending_reason(&self) -> Option<&str> {
        self.ending_reason.as_deref()
    }
    /// The single pending approval a modal should show, if any (the oldest).
    pub fn active_approval(&self) -> Option<&ApprovalRequest> {
        self.pending_approvals.first()
    }

    // ---- snapshot application (attach / reconnect) -----------------------

    /// Replace the entire view with a canonical daemon snapshot.
    ///
    /// Used on first attach and whenever the client recovers from a gap. The
    /// snapshot's `seq` becomes the new watermark, so subsequent in-order
    /// events resume cleanly from there.
    pub fn apply_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.session_id = snapshot.session_id;
        self.last_seq = snapshot.seq;
        self.turn_status = snapshot.turn_status;
        self.transcript = snapshot
            .transcript
            .into_iter()
            .map(transcript_entry_to_line)
            .collect();
        self.tool_calls = snapshot.tool_calls;
        self.pending_approvals = snapshot.pending_approvals;
        self.runtime_options = snapshot.runtime_options;
        self.context_usage = snapshot.context_usage;
        // A snapshot never carries "session already ended"; `SessionEnding`
        // is delivered as an event, so leave `ending_reason` untouched.
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
        self.apply_event_body(event);
        ApplyOutcome::Applied
    }

    fn apply_event_body(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::UserMessage { text } => {
                self.close_open_line();
                self.transcript
                    .push(ViewLine::committed(TranscriptRole::User, text));
            }
            SessionEvent::AssistantDelta { text } => {
                self.append_streaming(TranscriptRole::Assistant, &text);
            }
            SessionEvent::ThoughtDelta { text } => {
                self.append_streaming(TranscriptRole::Thought, &text);
            }
            SessionEvent::AssistantDone { outcome } => {
                self.close_open_line();
                if let Some(note) = outcome_note(&outcome) {
                    self.transcript
                        .push(ViewLine::committed(TranscriptRole::System, note));
                }
            }
            SessionEvent::ToolCallStarted {
                id,
                name,
                title,
                input_summary: _,
            } => {
                let call = ToolCallState {
                    id: id.clone(),
                    name,
                    title,
                    status: ToolCallStateStatus::Pending,
                    output: None,
                };
                match self.tool_calls.iter_mut().find(|c| c.id == id) {
                    Some(existing) => *existing = call,
                    None => self.tool_calls.push(call),
                }
            }
            SessionEvent::ToolCallUpdated { id, status } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|c| c.id == id) {
                    call.status = status;
                }
            }
            SessionEvent::ToolCallFinished { id, ok, output } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|c| c.id == id) {
                    call.status = if ok {
                        ToolCallStateStatus::Completed
                    } else {
                        ToolCallStateStatus::Failed
                    };
                    call.output = Some(output);
                }
            }
            SessionEvent::ApprovalRequested { request } => {
                if !self.pending_approvals.iter().any(|a| a.id == request.id) {
                    self.pending_approvals.push(request);
                }
            }
            SessionEvent::ApprovalResolved { approval_id, .. } => {
                self.pending_approvals.retain(|a| a.id != approval_id);
            }
            SessionEvent::RuntimeOptionsChanged { options } => {
                self.runtime_options = options;
            }
            SessionEvent::ContextUsageChanged { usage } => {
                self.context_usage = Some(usage);
            }
            SessionEvent::TurnStatusChanged { status } => {
                self.turn_status = status;
            }
            SessionEvent::SessionEnding { reason } => {
                self.close_open_line();
                self.ending_reason = Some(reason);
            }
        }
    }

    /// Append streamed text to the trailing open line if it matches `role`;
    /// otherwise close any open line and start a new open one.
    fn append_streaming(&mut self, role: TranscriptRole, text: &str) {
        if let Some(last) = self.transcript.last_mut() {
            if last.open && last.role == role {
                last.text.push_str(text);
                return;
            }
        }
        self.close_open_line();
        self.transcript.push(ViewLine {
            role,
            text: text.to_string(),
            open: true,
        });
    }

    /// Mark the trailing line committed if it is currently open. Only the last
    /// line can ever be open, so this is sufficient to "seal" streaming state.
    fn close_open_line(&mut self) {
        if let Some(last) = self.transcript.last_mut() {
            last.open = false;
        }
    }
}

fn transcript_entry_to_line(entry: TranscriptEntry) -> ViewLine {
    ViewLine::committed(entry.role, entry.text)
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
    use crate::session_protocol::ContextUsage;

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
            ev(&mut s, 1, SessionEvent::UserMessage { text: "hi".into() }),
            ApplyOutcome::Applied
        );
        ev(
            &mut s,
            2,
            SessionEvent::AssistantDelta {
                text: "Hel".into(),
            },
        );
        ev(
            &mut s,
            3,
            SessionEvent::AssistantDelta {
                text: "lo!".into(),
            },
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
    fn duplicate_sequence_is_ignored() {
        let mut s = ViewState::new("sess-1");
        ev(&mut s, 1, SessionEvent::UserMessage { text: "a".into() });
        // Replayed frame.
        assert_eq!(
            ev(&mut s, 1, SessionEvent::UserMessage { text: "a".into() }),
            ApplyOutcome::Duplicate
        );
        assert_eq!(s.transcript().len(), 1);
        assert_eq!(s.last_seq(), 1);
    }

    #[test]
    fn forward_gap_is_reported_and_not_applied() {
        let mut s = ViewState::new("sess-1");
        ev(&mut s, 1, SessionEvent::UserMessage { text: "a".into() });
        // seq 3 arrives before seq 2.
        assert_eq!(
            ev(
                &mut s,
                3,
                SessionEvent::AssistantDelta { text: "x".into() }
            ),
            ApplyOutcome::Gap { expected_seq: 2 }
        );
        // Nothing changed; watermark held at 1.
        assert_eq!(s.last_seq(), 1);
        assert_eq!(s.transcript().len(), 1);
        // The missing frame can still be applied in order.
        assert_eq!(
            ev(
                &mut s,
                2,
                SessionEvent::AssistantDelta { text: "x".into() }
            ),
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
        assert_eq!(roles, vec![TranscriptRole::Thought, TranscriptRole::Assistant]);
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
                ok: true,
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
                ok: false,
                output: "boom".into(),
            },
        );
        assert_eq!(s.tool_calls()[0].status, ToolCallStateStatus::Failed);
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
        ev(&mut s, 1, SessionEvent::UserMessage { text: "stale".into() });

        let snap = SessionSnapshot {
            session_id: "sess-42".into(),
            seq: 10,
            turn_status: TurnStatus::Idle,
            transcript: vec![
                TranscriptEntry {
                    id: 1,
                    role: TranscriptRole::User,
                    text: "question".into(),
                },
                TranscriptEntry {
                    id: 2,
                    role: TranscriptRole::Assistant,
                    text: "answer".into(),
                },
            ],
            tool_calls: Vec::new(),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: None,
        };
        s.apply_snapshot(snap);
        assert_eq!(s.session_id(), "sess-42");
        assert_eq!(s.last_seq(), 10);
        assert_eq!(s.transcript().len(), 2);

        // A pre-snapshot sequence is now a duplicate.
        assert_eq!(
            ev(&mut s, 5, SessionEvent::UserMessage { text: "x".into() }),
            ApplyOutcome::Duplicate
        );
        // The next in-order event (seq 11) applies on top of the snapshot.
        assert_eq!(
            ev(
                &mut s,
                11,
                SessionEvent::UserMessage {
                    text: "follow-up".into()
                }
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(s.transcript().len(), 3);
        assert_eq!(s.last_seq(), 11);
    }
}
