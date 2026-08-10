use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::oneshot;

use crate::agent::AgentSession;
use crate::compaction::CompactionPolicy;
use crate::providers::Message;
use crate::session_protocol::{
    ApprovalDecision, ApprovalRequest, AssistantOutcome, ClientCapability, SessionEvent, TurnStatus,
};
use crate::session_store::SessionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnError {
    Busy,
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("session is busy"),
        }
    }
}

impl std::error::Error for TurnError {}

pub enum SessionPromptOutcome {
    Completed(Box<crate::agent::TurnResult>),
    Cancelled,
}

pub struct SessionPromptExecution {
    pub outcome: SessionPromptOutcome,
    pub context_window: Option<u64>,
    pub cumulative_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPromptError {
    Busy,
    Stopped,
    DuplicateRequest(String),
    Model(String),
}

impl std::fmt::Display for SessionPromptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("session is busy"),
            Self::Stopped => formatter.write_str("session is stopped"),
            Self::DuplicateRequest(id) => {
                write!(formatter, "duplicate client user message id '{id}'")
            }
            Self::Model(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for SessionPromptError {}

fn error_has_token(error: &str, token: &str) -> bool {
    error
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| part == token)
}

fn error_has_http_status(error: &str, status: &str) -> bool {
    if error_has_token(error, status) {
        return true;
    }
    let compact: String = error
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    ["http", "api", "status"].iter().any(|prefix| {
        let needle = format!("{prefix}{status}");
        compact.match_indices(&needle).any(|(index, _)| {
            compact[index + needle.len()..]
                .chars()
                .next()
                .is_none_or(|character| !character.is_ascii_digit())
        })
    })
}

pub(crate) fn safe_provider_error_message(
    context_overflow: bool,
    error: Option<&str>,
) -> &'static str {
    let error = error.unwrap_or_default().to_ascii_lowercase();
    let normalized = error
        .replace(['_', '-', '`'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let invalid_tool_history = error_has_http_status(&error, "400")
        && normalized.contains("tool use ids were found without tool result blocks");
    if context_overflow
        || normalized.contains("prompt is too long")
        || normalized.contains("exceed context limit")
        || normalized.contains("maximum context length")
        || normalized.contains("context overflow")
        || normalized.contains("context length exceeded")
        || normalized.contains("context window exceeded")
        || normalized.contains("context window was exceeded")
    {
        "Provider rejected the prompt because the context window was exceeded."
    } else if invalid_tool_history {
        "Provider rejected invalid tool-call history. Restart/reload the session or use /clear."
    } else if error_has_http_status(&error, "402")
        || normalized.contains("insufficient credit")
        || normalized.contains("payment required")
    {
        "Provider billing/credit issue (HTTP 402). Check the provider account balance."
    } else if error_has_http_status(&error, "401")
        || normalized.contains("authentication")
        || normalized.contains("invalid api key")
        || normalized.contains("unauthorized")
        || normalized.contains("not authorized")
    {
        "Provider authentication failed (HTTP 401)."
    } else if error_has_http_status(&error, "403")
        || normalized.contains("permission denied")
        || normalized.contains("forbidden")
        || normalized.contains("authorization failed")
        || normalized.contains("authorization error")
    {
        "Provider authorization failed (HTTP 403)."
    } else if error_has_http_status(&error, "429") || normalized.contains("rate limit") {
        "Provider rate limit exceeded (HTTP 429)."
    } else if normalized.contains("timeout")
        || normalized.contains("timed out")
        || normalized.contains("time out")
    {
        "Provider request timed out."
    } else if normalized.contains("network")
        || normalized.contains("connection")
        || normalized.contains("upstream")
    {
        "Provider network request failed."
    } else if normalized.contains("parse") || normalized.contains("decode") {
        "Provider returned an invalid response."
    } else if normalized.contains("stream error")
        || normalized.contains("stream failed")
        || normalized.contains("response stream")
    {
        "Provider response stream failed."
    } else {
        "Provider request failed."
    }
}

pub(crate) fn canonical_assistant_outcome(turn: &crate::agent::TurnResult) -> AssistantOutcome {
    match turn.stop_reason {
        crate::providers::StopReason::Error => AssistantOutcome::Errored {
            context_overflow: turn.context_overflow,
            message: safe_provider_error_message(
                turn.context_overflow,
                turn.error_message.as_deref(),
            )
            .to_string(),
        },
        crate::providers::StopReason::Refusal => AssistantOutcome::Refused,
        crate::providers::StopReason::Aborted => AssistantOutcome::Aborted,
        crate::providers::StopReason::MaxTokens => AssistantOutcome::MaxTokens,
        crate::providers::StopReason::EndTurn | crate::providers::StopReason::ToolUse => {
            AssistantOutcome::Completed
        }
    }
}

pub(crate) const RAW_ERROR_LOG_CAP: usize = 500;

pub(crate) fn sanitize_provider_error(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(RAW_ERROR_LOG_CAP) + 16);
    let mut truncated = false;
    for token in raw.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim_end();
        let lower = trimmed.to_ascii_lowercase();
        let looks_secret = lower.starts_with("bearer")
            || lower.starts_with("authorization")
            || lower.starts_with("sk-")
            || lower.starts_with("or-")
            || (trimmed.len() >= 20
                && trimmed.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_".contains(character)
                }));
        if looks_secret {
            out.push_str("[REDACTED]");
            out.push_str(&token[trimmed.len()..]);
        } else {
            out.push_str(token);
        }
        if out.len() >= RAW_ERROR_LOG_CAP {
            truncated = true;
            break;
        }
    }
    if truncated {
        while !out.is_char_boundary(RAW_ERROR_LOG_CAP.min(out.len())) {
            out.pop();
        }
        out.truncate(RAW_ERROR_LOG_CAP.min(out.len()));
        out.push_str("…[truncated]");
    }
    out
}

pub(crate) fn log_raw_provider_error(
    session_id: impl std::fmt::Display,
    class: &str,
    error: Option<&str>,
) {
    if let Some(raw) = error {
        tracing::debug!(
            target: "daimonos::session_core",
            event = "provider_request_raw",
            session_id = %session_id,
            class,
            raw_error = sanitize_provider_error(raw),
        );
    }
}

pub(crate) fn canonical_assistant_outcome_with_logging(
    session_id: impl std::fmt::Display,
    turn: &crate::agent::TurnResult,
) -> AssistantOutcome {
    let outcome = canonical_assistant_outcome(turn);
    if let AssistantOutcome::Errored { message, .. } = &outcome {
        tracing::Span::current().record("error.type", "provider_error");
        tracing::warn!(
            target: "daimonos::session_core",
            event = "provider_request_failed",
            session_id = %session_id,
            class = message,
        );
        log_raw_provider_error(session_id, message, turn.error_message.as_deref());
    }
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventError {
    SequenceExhausted,
    HandlerPanicked,
    HandlerLimitReached { max: usize },
}

pub type SessionEventHandler = std::sync::Arc<dyn Fn(u64, SessionEvent) + Send + Sync + 'static>;

/// Transport-independent projection of canonical session events.
///
/// The router assigns session-local monotonic sequence numbers before handing
/// events to the configured adapter. Reliable replay/ring retention is layered
/// on this sequence in Vikunja #1100; this layer owns only canonical ordering.
pub struct SessionEventRouter {
    dispatch: StdMutex<()>,
    sequence: StdMutex<u64>,
    handlers: StdMutex<SessionEventHandlers>,
    replay: StdMutex<VecDeque<(u64, SessionEvent)>>,
    max_replay_events: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReplay {
    Available {
        events: Vec<(u64, SessionEvent)>,
        latest_seq: u64,
    },
    SnapshotRequired {
        latest_seq: u64,
    },
}

struct SessionEventHandlers {
    next_id: u64,
    fixed_count: usize,
    entries: HashMap<u64, SessionEventHandler>,
}

pub struct SessionEventSubscription {
    router: std::sync::Weak<SessionEventRouter>,
    handler_id: u64,
}

impl SessionEventRouter {
    pub fn new(handler: Option<SessionEventHandler>) -> Self {
        Self::new_with_replay(handler, 0)
    }

    pub fn new_with_replay(handler: Option<SessionEventHandler>, max_replay_events: usize) -> Self {
        let fixed_count = usize::from(handler.is_some());
        let mut entries = HashMap::new();
        if let Some(handler) = handler {
            entries.insert(0, handler);
        }
        Self {
            dispatch: StdMutex::new(()),
            sequence: StdMutex::new(0),
            handlers: StdMutex::new(SessionEventHandlers {
                next_id: 1,
                fixed_count,
                entries,
            }),
            replay: StdMutex::new(VecDeque::new()),
            max_replay_events,
        }
    }

    pub fn emit(&self, event: SessionEvent) -> Result<u64, SessionEventError> {
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = {
            let mut current = self
                .sequence
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *current = current
                .checked_add(1)
                .ok_or(SessionEventError::SequenceExhausted)?;
            *current
        };
        if self.max_replay_events > 0 {
            let mut replay = self
                .replay
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            replay.push_back((sequence, event.clone()));
            while replay.len() > self.max_replay_events {
                replay.pop_front();
            }
        }
        let handlers: Vec<SessionEventHandler> = self
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .values()
            .cloned()
            .collect();
        let mut handler_panicked = false;
        for handler in handlers {
            let event = event.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handler(sequence, event);
            }))
            .is_err()
            {
                handler_panicked = true;
            }
        }
        if handler_panicked {
            return Err(SessionEventError::HandlerPanicked);
        }
        Ok(sequence)
    }

    pub fn subscribe(
        self: &Arc<Self>,
        max_handlers: usize,
        handler: SessionEventHandler,
    ) -> Result<SessionEventSubscription, SessionEventError> {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dynamic_count = handlers.entries.len().saturating_sub(handlers.fixed_count);
        if dynamic_count >= max_handlers {
            return Err(SessionEventError::HandlerLimitReached { max: max_handlers });
        }
        let handler_id = handlers.next_id;
        handlers.next_id = handlers
            .next_id
            .checked_add(1)
            .ok_or(SessionEventError::SequenceExhausted)?;
        handlers.entries.insert(handler_id, handler);
        Ok(SessionEventSubscription {
            router: Arc::downgrade(self),
            handler_id,
        })
    }

    pub fn subscribe_and_capture<T>(
        self: &Arc<Self>,
        max_handlers: usize,
        handler: SessionEventHandler,
        capture: impl FnOnce() -> T,
    ) -> Result<(SessionEventSubscription, T), SessionEventError> {
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let subscription = self.subscribe(max_handlers, handler)?;
        let captured = capture();
        Ok((subscription, captured))
    }

    pub fn latest_sequence(&self) -> u64 {
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .sequence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn replay_since(&self, last_seen_seq: u64) -> SessionReplay {
        let _dispatch = self
            .dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let latest_seq = *self
            .sequence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last_seen_seq > latest_seq {
            return SessionReplay::SnapshotRequired { latest_seq };
        }
        if last_seen_seq == latest_seq {
            return SessionReplay::Available {
                events: Vec::new(),
                latest_seq,
            };
        }
        let replay = self
            .replay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some((earliest_seq, _)) = replay.front() else {
            return SessionReplay::SnapshotRequired { latest_seq };
        };
        if last_seen_seq.saturating_add(1) < *earliest_seq {
            return SessionReplay::SnapshotRequired { latest_seq };
        }
        SessionReplay::Available {
            events: replay
                .iter()
                .filter(|(seq, _)| *seq > last_seen_seq)
                .cloned()
                .collect(),
            latest_seq,
        }
    }
}

impl Drop for SessionEventSubscription {
    fn drop(&mut self) {
        let Some(router) = self.router.upgrade() else {
            return;
        };
        router
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .remove(&self.handler_id);
    }
}

impl Default for SessionEventRouter {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Compaction policy plus whether its context window follows the active model.
#[derive(Clone)]
pub struct SessionCompaction {
    pub(crate) policy: Option<CompactionPolicy>,
    pub(crate) follows_model_window: bool,
}

impl SessionCompaction {
    pub fn new(policy: Option<CompactionPolicy>, follows_model_window: bool) -> Self {
        Self {
            policy,
            follows_model_window,
        }
    }

    pub fn policy_for(
        &self,
        model: &str,
        context_window: Option<u64>,
    ) -> Result<Option<CompactionPolicy>, String> {
        let Some(mut policy) = self.policy.clone() else {
            return Ok(None);
        };
        if self.follows_model_window {
            let context_window = context_window.ok_or_else(|| {
                format!(
                    "could not determine the context window for model '{model}' from the provider"
                )
            })?;
            if policy.output_reservation >= context_window {
                return Err(format!(
                    "DAIMONOS_AGENT_OUTPUT_RESERVATION ({}) must be smaller than the \
                     provider-reported context window ({context_window}) for model '{model}'",
                    policy.output_reservation
                ));
            }
            policy.context_window = context_window;
        }
        Ok(Some(policy))
    }
}

#[derive(Clone)]
pub struct SessionPersistence {
    session_id: String,
    store: SessionStore,
    state: Arc<StdMutex<SessionPersistenceState>>,
}

#[derive(Default)]
struct SessionPersistenceState {
    deleted: bool,
}

impl SessionPersistence {
    pub fn new(session_id: impl Into<String>, store: SessionStore) -> Self {
        Self {
            session_id: session_id.into(),
            store,
            state: Arc::new(StdMutex::new(SessionPersistenceState::default())),
        }
    }

    fn save(
        &self,
        model: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
    ) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.deleted {
            return;
        }
        self.store.save_acp(
            &self.session_id,
            model,
            messages,
            cwd,
            client_user_message_ids,
            assistant_outcomes,
        );
    }

    fn delete(&self) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.deleted = true;
        self.store.delete(&self.session_id)
    }
}

/// Daemon-owned, transport-independent state for one live agent session.
///
/// Provider, tools, safety hooks, history, usage, and runtime config live inside
/// `AgentSession`; this core adds lifecycle serialization, active-turn routing,
/// approvals, persistence identity, model/window state, and canonical events.
pub struct SessionCore {
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
    pub(crate) session: tokio::sync::Mutex<AgentSession>,
    pub(crate) turn: TurnController,
    pub(crate) approvals: Arc<ApprovalBroker>,
    pub(crate) current_model: StdMutex<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) client_user_message_ids: tokio::sync::Mutex<Vec<String>>,
    pub(crate) assistant_outcomes: StdMutex<Vec<AssistantOutcome>>,
    pub(crate) compaction: SessionCompaction,
    pub(crate) context_windows: tokio::sync::Mutex<HashMap<String, u64>>,
    pub(crate) events: Arc<SessionEventRouter>,
    pub(crate) tool_lifecycle: Arc<CanonicalToolLifecycle>,
    persistence: Option<SessionPersistence>,
}

impl SessionCore {
    pub async fn has_completed_request(&self, request_id: &str) -> bool {
        self.client_user_message_ids
            .lock()
            .await
            .iter()
            .any(|existing| existing == request_id)
    }

    pub fn begin_turn(&self) -> Result<SessionTurn<'_>, TurnError> {
        let active = self.turn.begin()?;
        let _ = self.events.emit(SessionEvent::TurnStatusChanged {
            status: crate::session_protocol::TurnStatus::Running,
        });
        Ok(SessionTurn {
            active,
            events: &self.events,
        })
    }

    /// Cancel the active turn, emitting `Cancelling` before the notification
    /// that unwinds it. Returns false when no turn is active.
    ///
    /// Emitting from here rather than from the caller is what makes the sequence
    /// deterministic: the terminal event comes from `SessionTurn::drop` on the
    /// turn's own task, which the notification triggers, so a `Cancelling`
    /// emitted afterwards could arrive *after* the turn had already ended and
    /// leave the stream resting on `Cancelling` for good.
    pub fn cancel_turn(&self) -> bool {
        self.turn.cancel_with(|| {
            let _ = self.events.emit(SessionEvent::TurnStatusChanged {
                status: crate::session_protocol::TurnStatus::Cancelling,
            });
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: AgentSession,
        current_model: String,
        cwd: PathBuf,
        compaction: SessionCompaction,
        context_windows: HashMap<String, u64>,
        approvals: Arc<ApprovalBroker>,
        persistence: Option<SessionPersistence>,
        events: Arc<SessionEventRouter>,
        tool_lifecycle: Arc<CanonicalToolLifecycle>,
    ) -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            session: tokio::sync::Mutex::new(session),
            turn: TurnController::default(),
            approvals,
            current_model: StdMutex::new(current_model),
            cwd,
            client_user_message_ids: tokio::sync::Mutex::new(Vec::new()),
            assistant_outcomes: StdMutex::new(Vec::new()),
            compaction,
            context_windows: tokio::sync::Mutex::new(context_windows),
            events,
            tool_lifecycle,
            persistence,
        }
    }

    pub async fn prepare_model(
        &self,
        session: &mut AgentSession,
        model: &str,
    ) -> Result<Option<u64>, String> {
        let cached = self.context_windows.lock().await.get(model).copied();
        let context_window = match cached {
            Some(window) => Some(window),
            None => {
                let resolved = session
                    .context_window(model)
                    .await
                    .filter(|&window| window > 0);
                if let Some(window) = resolved {
                    self.context_windows
                        .lock()
                        .await
                        .insert(model.to_string(), window);
                }
                resolved
            }
        };
        let policy = self.compaction.policy_for(model, context_window)?;
        session.set_model(model);
        session.set_compaction(policy);
        Ok(context_window.or_else(|| {
            (!self.compaction.follows_model_window)
                .then(|| {
                    self.compaction
                        .policy
                        .as_ref()
                        .map(|policy| policy.context_window)
                })
                .flatten()
        }))
    }

    pub fn context_usage(
        &self,
        used_tokens: u64,
        context_window: Option<u64>,
    ) -> crate::session_protocol::ContextUsage {
        let (reservation, high_water) = self
            .compaction
            .policy
            .as_ref()
            .map(|policy| {
                let budget = policy.budget();
                let valid_fraction = policy.high_water.is_finite()
                    && (0.0..=1.0).contains(&policy.high_water)
                    && budget > 0;
                let high_water =
                    valid_fraction.then(|| (policy.high_water * budget as f64).round() as u64);
                (policy.output_reservation, high_water)
            })
            .unwrap_or((0, None));
        crate::session_protocol::ContextUsage::new(
            used_tokens,
            context_window,
            reservation,
            high_water,
        )
    }

    pub fn persist(&self, model: &str, messages: &[Message], client_user_message_ids: &[String]) {
        if let Some(persistence) = &self.persistence {
            let outcomes = self
                .assistant_outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            persistence.save(
                model,
                messages,
                &self.cwd,
                client_user_message_ids,
                &outcomes,
            );
        }
    }

    pub fn delete_persisted(&self) -> std::io::Result<bool> {
        match &self.persistence {
            Some(persistence) => persistence.delete(),
            None => Ok(false),
        }
    }

    pub async fn initial_snapshot(
        &self,
        session_id: String,
        max_entries: usize,
    ) -> crate::session_protocol::SessionSnapshot {
        use crate::providers::{ContentBlock, Role};
        use crate::session_protocol::{
            ToolCallState, ToolCallStateStatus, TranscriptEntry, TranscriptRole,
        };

        let session = self.session.lock().await;
        let mut transcript = Vec::new();
        let mut tool_calls: Vec<ToolCallState> = Vec::new();
        let mut next_transcript_id = 1_u64;
        for message in session.history() {
            for block in &message.content {
                match block {
                    ContentBlock::Text(text) => {
                        transcript.push(TranscriptEntry {
                            id: next_transcript_id,
                            role: match message.role {
                                Role::User => TranscriptRole::User,
                                Role::Assistant => TranscriptRole::Assistant,
                            },
                            text: text.clone(),
                            outcome: None,
                        });
                        next_transcript_id = next_transcript_id.saturating_add(1);
                    }
                    ContentBlock::Thinking(text) => {
                        transcript.push(TranscriptEntry {
                            id: next_transcript_id,
                            role: TranscriptRole::Thought,
                            text: text.clone(),
                            outcome: None,
                        });
                        next_transcript_id = next_transcript_id.saturating_add(1);
                    }
                    ContentBlock::ToolCall { id, name, .. } => {
                        tool_calls.push(ToolCallState {
                            id: id.clone(),
                            name: name.clone(),
                            title: name.clone(),
                            status: ToolCallStateStatus::InProgress,
                            output: None,
                        });
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let status = if content == crate::agent::INTERRUPTED_TOOL_RESULT {
                            ToolCallStateStatus::Cancelled
                        } else if *is_error {
                            ToolCallStateStatus::Failed
                        } else {
                            ToolCallStateStatus::Completed
                        };
                        apply_restored_tool_result(
                            &mut tool_calls,
                            tool_use_id,
                            status,
                            self.tool_lifecycle.project_output(content),
                        );
                    }
                    ContentBlock::Image { .. } | ContentBlock::ProviderState { .. } => {}
                }
            }
        }
        drop(session);
        let outcomes = self
            .assistant_outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        apply_persisted_outcomes(&mut transcript, &outcomes);
        for call in &mut tool_calls {
            if call.status == ToolCallStateStatus::InProgress {
                call.status = ToolCallStateStatus::Cancelled;
            }
        }
        trim_oldest(&mut transcript, max_entries.max(1));
        trim_oldest(&mut tool_calls, max_entries.max(1));
        crate::session_protocol::SessionSnapshot {
            session_id,
            seq: self.events.latest_sequence(),
            turn_status: if self.turn.is_active() {
                TurnStatus::Running
            } else {
                TurnStatus::Idle
            },
            transcript,
            tool_calls,
            pending_approvals: self.approvals.pending(),
            runtime_options: Vec::new(),
            context_usage: None,
            history_truncated: false,
        }
    }

    /// Resolve every approval and canonical tool call whose normal completion
    /// path was dropped with a cancelled prompt or retry.
    pub fn cleanup_cancelled_turn(&self) {
        for resolution in self.approvals.cancel_all("session_cancelled") {
            self.tool_lifecycle.apply_approval_side_effects(&resolution);
            let _ = self.events.emit(SessionEvent::ApprovalResolved {
                approval_id: resolution.approval_id,
                decision: resolution.decision,
                resolved_by: resolution.resolved_by,
            });
        }
        self.tool_lifecycle.cancel_all();
    }

    /// Execute one provider/tool turn without depending on any frontend
    /// transport. Adapters supply only presentation cleanup and the canonical
    /// terminal-outcome mapping; all state, cancellation, persistence, usage,
    /// and event ordering remain daemon-owned.
    #[allow(clippy::too_many_arguments)]
    pub async fn prompt<C, M>(
        &self,
        user_message: Message,
        canonical_user_text: String,
        client_user_message_id: Option<String>,
        assistant_prefix: Option<String>,
        on_cancel: C,
        outcome_mapper: M,
    ) -> Result<SessionPromptExecution, SessionPromptError>
    where
        C: FnOnce(),
        M: Fn(&crate::agent::TurnResult) -> crate::session_protocol::AssistantOutcome,
    {
        let active_turn = self.begin_turn().map_err(|_| SessionPromptError::Busy)?;
        let event_request_id = client_user_message_id.clone();
        self.prompt_with_active_turn(
            active_turn,
            user_message,
            canonical_user_text,
            client_user_message_id,
            event_request_id,
            assistant_prefix,
            on_cancel,
            outcome_mapper,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prompt_with_active_turn<C, M>(
        &self,
        active_turn: SessionTurn<'_>,
        user_message: Message,
        canonical_user_text: String,
        client_user_message_id: Option<String>,
        event_request_id: Option<String>,
        assistant_prefix: Option<String>,
        on_cancel: C,
        outcome_mapper: M,
    ) -> Result<SessionPromptExecution, SessionPromptError>
    where
        C: FnOnce(),
        M: Fn(&crate::agent::TurnResult) -> crate::session_protocol::AssistantOutcome,
    {
        let mut agent_session = self.session.lock().await;
        {
            let mut client_ids = self.client_user_message_ids.lock().await;
            align_client_user_message_ids(&mut client_ids, agent_session.user_turn_count());
            if let Some(id) = client_user_message_id.as_deref() {
                if client_ids.iter().any(|existing| existing == id) {
                    return Err(SessionPromptError::DuplicateRequest(id.to_string()));
                }
            }
        }

        let model = self
            .current_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let context_window = self
            .prepare_model(&mut agent_session, &model)
            .await
            .map_err(SessionPromptError::Model)?;
        let _ = self.events.emit(SessionEvent::UserMessage {
            text: canonical_user_text,
            request_id: event_request_id,
        });
        if let Some(prefix) = assistant_prefix.as_ref() {
            let _ = self.events.emit(SessionEvent::AssistantDelta {
                text: prefix.clone(),
            });
        }

        let turn = tokio::select! {
            turn = agent_session.prompt_message(user_message) => Some(turn),
            _ = active_turn.cancelled() => None,
        };
        if turn.is_some() {
            active_turn.mark_completed();
        } else {
            self.cleanup_cancelled_turn();
            on_cancel();
        }
        if turn.is_some() {
            if let Some(prefix) = assistant_prefix {
                if let Err(error) = agent_session.insert_assistant_turn_prefix(prefix) {
                    tracing::error!(
                        target: "daimonos::session_core",
                        event = "assistant_turn_prefix_insert_failed",
                        error = %error,
                    );
                }
            }
        }

        let history_snapshot = turn.as_ref().map(|_| agent_session.history().to_vec());
        let cumulative_cost_usd = agent_session.total_usage().cost.total_usd;
        let client_ids_snapshot = if turn.is_some() {
            let mut client_ids = self.client_user_message_ids.lock().await;
            client_ids.push(client_user_message_id.unwrap_or_default());
            align_client_user_message_ids(&mut client_ids, agent_session.user_turn_count());
            Some(client_ids.clone())
        } else {
            None
        };
        if turn.is_some() {
            let completed_before_current = agent_session.user_turn_count().saturating_sub(1);
            let mut outcomes = self
                .assistant_outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if outcomes.len() > completed_before_current {
                let excess = outcomes.len() - completed_before_current;
                outcomes.drain(..excess);
            }
        }
        drop(agent_session);

        if let Some(turn) = turn.as_ref() {
            let used_tokens = turn
                .last_call_usage
                .prompt_tokens()
                .saturating_add(turn.last_call_usage.output);
            let _ = self.events.emit(SessionEvent::ContextUsageChanged {
                usage: self.context_usage(used_tokens, context_window),
            });
            let outcome = outcome_mapper(turn);
            self.assistant_outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(outcome.clone());
            let _ = self.events.emit(SessionEvent::AssistantDone { outcome });
        }
        drop(active_turn);

        if let (Some(messages), Some(client_ids)) = (history_snapshot, client_ids_snapshot) {
            self.persist(&model, &messages, &client_ids);
        }

        Ok(SessionPromptExecution {
            outcome: match turn {
                Some(turn) => SessionPromptOutcome::Completed(Box::new(turn)),
                None => SessionPromptOutcome::Cancelled,
            },
            context_window,
            cumulative_cost_usd,
        })
    }
}

fn trim_oldest<T>(entries: &mut Vec<T>, max_entries: usize) {
    let excess = entries.len().saturating_sub(max_entries);
    if excess > 0 {
        entries.drain(..excess);
    }
}

fn apply_persisted_outcomes(
    transcript: &mut Vec<crate::session_protocol::TranscriptEntry>,
    outcomes: &[AssistantOutcome],
) {
    use crate::session_protocol::{TranscriptEntry, TranscriptRole};

    let mut rebuilt = Vec::with_capacity(transcript.len() + outcomes.len());
    let mut turn_start = None;
    let mut outcome_index = 0;
    let mut next_id = transcript
        .iter()
        .map(|entry| entry.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let finalize = |entries: &mut Vec<TranscriptEntry>,
                    start: usize,
                    outcome: Option<&AssistantOutcome>,
                    next_id: &mut u64| {
        let Some(outcome) = outcome else {
            return;
        };
        if let Some(entry) = entries[start..]
            .iter_mut()
            .rev()
            .find(|entry| entry.role == TranscriptRole::Assistant)
        {
            entry.outcome = Some(outcome.clone());
        } else {
            entries.push(TranscriptEntry {
                id: *next_id,
                role: TranscriptRole::Assistant,
                text: String::new(),
                outcome: Some(outcome.clone()),
            });
            *next_id = next_id.saturating_add(1);
        }
    };
    for entry in transcript.drain(..) {
        if entry.role == TranscriptRole::User {
            if let Some(start) = turn_start.take() {
                finalize(
                    &mut rebuilt,
                    start,
                    outcomes.get(outcome_index),
                    &mut next_id,
                );
                outcome_index += 1;
            }
            turn_start = Some(rebuilt.len());
        }
        rebuilt.push(entry);
    }
    if let Some(start) = turn_start {
        finalize(
            &mut rebuilt,
            start,
            outcomes.get(outcome_index),
            &mut next_id,
        );
    }
    *transcript = rebuilt;
}

fn apply_restored_tool_result(
    tool_calls: &mut [crate::session_protocol::ToolCallState],
    tool_use_id: &str,
    status: crate::session_protocol::ToolCallStateStatus,
    output: String,
) {
    if let Some(call) = tool_calls.iter_mut().find(|call| {
        call.id == tool_use_id
            && call.status == crate::session_protocol::ToolCallStateStatus::InProgress
    }) {
        call.status = status;
        call.output = Some(output);
    }
}

pub(crate) fn align_client_user_message_ids(ids: &mut Vec<String>, user_turn_count: usize) {
    if ids.len() > user_turn_count {
        let excess = ids.len() - user_turn_count;
        ids.drain(..excess);
    } else if ids.len() < user_turn_count {
        let mut padding = vec![String::new(); user_turn_count - ids.len()];
        padding.append(ids);
        *ids = padding;
    }
}

pub struct SessionTurn<'a> {
    active: ActiveTurn<'a>,
    events: &'a SessionEventRouter,
}

impl SessionTurn<'_> {
    pub async fn cancelled(&self) {
        self.active.cancelled().await;
    }

    /// Claim completion, so a cancel arriving during post-turn bookkeeping
    /// (persistence snapshots, id alignment) cannot relabel a finished turn as
    /// cancelled. Call as soon as the turn's work has completed.
    pub fn mark_completed(&self) {
        self.active.complete();
    }
}

impl Drop for SessionTurn<'_> {
    fn drop(&mut self) {
        // Drop is the single terminal emitter for a turn, mirroring the
        // single-terminal claim the tool-call lifecycle already uses: it is the
        // one place guaranteed to run on every exit path — normal return, `?`,
        // panic, or a dropped future on cancellation.
        //
        // Which terminal event depends on how the turn ended. Emitting `Idle`
        // unconditionally left a cancelled turn indistinguishable from a
        // completed one in the canonical stream.
        let status = if self.active.is_cancelled() {
            crate::session_protocol::TurnStatus::Cancelled
        } else {
            crate::session_protocol::TurnStatus::Idle
        };
        if let Err(error) = self.events.emit(SessionEvent::TurnStatusChanged { status }) {
            tracing::error!(
                target: "daimonos::session_core",
                event = "turn_idle_event_failed",
                error = ?error,
            );
        }
    }
}

/// The cancellation route for one turn, shared between the task running the turn
/// and whoever cancels it.
///
/// `outcome` is separate from the notification because `SessionTurn::drop`
/// decides the turn's terminal event and cannot `await`: a `Notify` carries no
/// readable "was this signalled" state, so the fact has to be recorded on its
/// own.
///
/// It is a claim, not a flag: exactly one of completion and cancellation wins
/// the `Live -> {Completed, Cancelled}` transition. Without the claim, a cancel
/// arriving between the prompt finishing and `SessionTurn` dropping marked a
/// *completed* turn cancelled — the canonical stream said `Cancelled` while the
/// ACP response said `EndTurn`.
#[derive(Default)]
struct TurnSignal {
    notify: tokio::sync::Notify,
    outcome: AtomicU8,
}

impl TurnSignal {
    const LIVE: u8 = 0;
    const COMPLETED: u8 = 1;
    const CANCELLED: u8 = 2;

    /// Claim `outcome` for this turn. Returns true if this call won the claim;
    /// false if the other outcome already holds it. Claiming the same outcome
    /// twice reports true (idempotent from the claimant's side).
    fn claim(&self, outcome: u8) -> bool {
        match self
            .outcome
            .compare_exchange(Self::LIVE, outcome, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => true,
            Err(current) => current == outcome,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.outcome.load(Ordering::SeqCst) == Self::CANCELLED
    }
}

/// Transport-independent ownership for one session's active turn.
///
/// The controller prevents a second prompt from replacing the first turn's
/// cancellation route. The active permit clears the route on every exit path,
/// including early returns and unwinding.
#[derive(Default)]
pub struct TurnController {
    active: StdMutex<Option<std::sync::Arc<TurnSignal>>>,
}

pub struct ActiveTurn<'a> {
    controller: &'a TurnController,
    signal: std::sync::Arc<TurnSignal>,
}

impl TurnController {
    pub fn begin(&self) -> Result<ActiveTurn<'_>, TurnError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.is_some() {
            return Err(TurnError::Busy);
        }
        let signal = std::sync::Arc::new(TurnSignal::default());
        *active = Some(std::sync::Arc::clone(&signal));
        Ok(ActiveTurn {
            controller: self,
            signal,
        })
    }

    #[cfg(test)]
    pub fn cancel(&self) -> bool {
        self.cancel_with(|| {})
    }

    /// Cancel the active turn, running `before_signal` after the turn is marked
    /// cancelled but *before* the notification that unwinds it.
    ///
    /// The ordering is the point. The terminal event comes from
    /// `SessionTurn::drop` on the task running the turn, which the notification
    /// triggers. Anything the canceller wants recorded ahead of that terminal
    /// event — a `Cancelling` status, say — has to be emitted before the notify,
    /// or it can land after the turn has already ended and leave the stream
    /// resting on a non-terminal status.
    pub fn cancel_with<F: FnOnce()>(&self, before_signal: F) -> bool {
        let signal = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(signal) = signal else {
            return false;
        };
        // Claim the outcome first. A turn that already completed refuses the
        // cancel outright: marking it cancelled after the fact would put a
        // `Cancelled` terminal event on a turn whose ACP response was `EndTurn`.
        // A repeat cancel is accepted (and re-notifies) but must not win a
        // second `Cancelling` emission, so the callback runs only on the state
        // transition itself.
        let already_cancelled = signal.is_cancelled();
        if !signal.claim(TurnSignal::CANCELLED) {
            return false;
        }
        if !already_cancelled {
            before_signal();
        }
        signal.notify.notify_one();
        true
    }

    pub fn is_active(&self) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

impl ActiveTurn<'_> {
    pub async fn cancelled(&self) {
        self.signal.notify.notified().await;
    }

    /// Whether this turn has been cancelled. Readable synchronously so `Drop`
    /// can pick the terminal event.
    fn is_cancelled(&self) -> bool {
        self.signal.is_cancelled()
    }

    /// Claim completion for this turn, so a later cancel cannot relabel it.
    /// Loses gracefully: if cancellation claimed first, the turn stays
    /// cancelled — the claim decides, whichever side gets there first.
    fn complete(&self) {
        let _ = self.signal.claim(TurnSignal::COMPLETED);
    }
}

impl Drop for ActiveTurn<'_> {
    fn drop(&mut self) {
        let mut active = self
            .controller
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active
            .as_ref()
            .is_some_and(|signal| std::sync::Arc::ptr_eq(signal, &self.signal))
        {
            *active = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalResolution {
    pub approval_id: String,
    pub tool: String,
    pub decision: ApprovalDecision,
    pub resolved_by: String,
}

pub struct RegisteredApproval {
    pub request: ApprovalRequest,
    pub receiver: oneshot::Receiver<ApprovalResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    NotPending,
    AlreadyResolved,
    MissingCapability(ClientCapability),
    AllowAlwaysUnavailable,
    IdExhausted,
}

struct PendingApproval {
    request: ApprovalRequest,
    sender: oneshot::Sender<ApprovalResolution>,
}

#[derive(Default)]
struct ApprovalState {
    pending: HashMap<String, PendingApproval>,
    /// Resolutions removed from `pending` but not yet claimed for canonical
    /// event emission. The waiting prompt and cancellation cleanup race to
    /// take each entry, guaranteeing exactly one `ApprovalResolved` event.
    unemitted: HashMap<String, ApprovalResolution>,
    next_id: u64,
}

pub struct ApprovalBroker {
    state: StdMutex<ApprovalState>,
    allow_always: bool,
    timeout: Option<std::time::Duration>,
    approve_once_clients: AtomicUsize,
    approve_always_clients: AtomicUsize,
    eligibility_changed: tokio::sync::Notify,
}

impl ApprovalBroker {
    pub fn new(allow_always: bool) -> Self {
        Self {
            state: StdMutex::new(ApprovalState::default()),
            allow_always,
            timeout: None,
            approve_once_clients: AtomicUsize::new(0),
            approve_always_clients: AtomicUsize::new(0),
            eligibility_changed: tokio::sync::Notify::new(),
        }
    }

    pub fn new_with_timeout(allow_always: bool, timeout: std::time::Duration) -> Self {
        Self {
            state: StdMutex::new(ApprovalState::default()),
            allow_always,
            timeout: Some(timeout),
            approve_once_clients: AtomicUsize::new(0),
            approve_always_clients: AtomicUsize::new(0),
            eligibility_changed: tokio::sync::Notify::new(),
        }
    }

    pub fn set_eligible_client_counts(&self, approve_once: usize, approve_always: usize) {
        self.approve_once_clients
            .store(approve_once, Ordering::Release);
        self.approve_always_clients
            .store(approve_always, Ordering::Release);
        self.eligibility_changed.notify_waiters();
    }

    pub(crate) fn has_eligible_client(&self, approval_id: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = state.pending.get(approval_id) else {
            return false;
        };
        self.approve_once_clients.load(Ordering::Acquire) > 0
            || pending.request.allow_always_available
                && self.approve_always_clients.load(Ordering::Acquire) > 0
    }

    /// Register one approval using a broker-generated, session-local monotonic
    /// id. Callers never choose ids, so an evicted/late response cannot collide
    /// with a newer request during this broker's lifetime.
    pub fn register(
        &self,
        mut request: ApprovalRequest,
    ) -> Result<RegisteredApproval, ApprovalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(ApprovalError::IdExhausted)?;
        request.id = format!("approval-{}", state.next_id);
        request.allow_always_available &= self.allow_always;
        let (sender, receiver) = oneshot::channel();
        state.pending.insert(
            request.id.clone(),
            PendingApproval {
                request: request.clone(),
                sender,
            },
        );
        Ok(RegisteredApproval { request, receiver })
    }

    pub fn pending(&self) -> Vec<ApprovalRequest> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending: Vec<_> = state
            .pending
            .values()
            .map(|pending| pending.request.clone())
            .collect();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }

    pub fn resolve(
        &self,
        approval_id: &str,
        resolved_by: &str,
        capabilities: &[ClientCapability],
        decision: ApprovalDecision,
    ) -> Result<ApprovalResolution, ApprovalError> {
        let required = match decision {
            ApprovalDecision::AllowAlways => ClientCapability::ApproveAlways,
            ApprovalDecision::AllowOnce | ApprovalDecision::Deny => ClientCapability::ApproveOnce,
        };
        if !capabilities.contains(&required) {
            return Err(ApprovalError::MissingCapability(required));
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = state.pending.get(approval_id) else {
            let known_sequence = approval_sequence(approval_id);
            return Err(if known_sequence <= state.next_id {
                ApprovalError::AlreadyResolved
            } else {
                ApprovalError::NotPending
            });
        };
        if decision == ApprovalDecision::AllowAlways && !pending.request.allow_always_available {
            return Err(ApprovalError::AllowAlwaysUnavailable);
        }
        let Some(pending) = state.pending.remove(approval_id) else {
            return Err(ApprovalError::NotPending);
        };
        let resolution = ApprovalResolution {
            approval_id: approval_id.to_string(),
            tool: pending.request.tool.clone(),
            decision,
            resolved_by: resolved_by.to_string(),
        };
        state
            .unemitted
            .insert(approval_id.to_string(), resolution.clone());
        let _ = pending.sender.send(resolution.clone());
        Ok(resolution)
    }

    pub fn cancel_all(&self, resolved_by: &str) -> Vec<ApprovalResolution> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending: Vec<_> = state.pending.drain().collect();
        pending.sort_by_key(|(approval_id, _)| approval_sequence(approval_id));
        let mut resolutions = Vec::with_capacity(pending.len());
        for (approval_id, pending) in pending {
            let resolution = ApprovalResolution {
                approval_id,
                tool: pending.request.tool,
                decision: ApprovalDecision::Deny,
                resolved_by: resolved_by.to_string(),
            };
            let _ = pending.sender.send(resolution.clone());
            resolutions.push(resolution);
        }
        resolutions.extend(state.unemitted.drain().map(|(_, resolution)| resolution));
        resolutions.sort_by_key(|resolution| approval_sequence(&resolution.approval_id));
        resolutions
    }

    fn take_unemitted(&self, approval_id: &str) -> Option<ApprovalResolution> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unemitted
            .remove(approval_id)
    }
}

fn approval_sequence(approval_id: &str) -> u64 {
    approval_id
        .strip_prefix("approval-")
        .and_then(|sequence| sequence.parse().ok())
        .unwrap_or(u64::MAX)
}

impl Drop for ApprovalBroker {
    fn drop(&mut self) {
        let _ = self.cancel_all("broker_drop");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalRequestError {
    Broker(ApprovalError),
    Event(SessionEventError),
    BrokerClosed,
}

/// Register one canonical approval, publish it to the active client adapter,
/// and wait for that adapter to resolve it through [`ApprovalBroker`].
///
/// No transport handle crosses this boundary. ACP, a local UDS client, and a
/// future Android client all observe the same `ApprovalRequested` event and
/// answer through the same broker.
pub async fn request_approval(
    broker: &ApprovalBroker,
    events: &SessionEventRouter,
    request: ApprovalRequest,
) -> Result<ApprovalResolution, ApprovalRequestError> {
    let registered = broker
        .register(request)
        .map_err(ApprovalRequestError::Broker)?;
    let approval_id = registered.request.id.clone();

    if let Err(error) = events.emit(SessionEvent::TurnStatusChanged {
        status: TurnStatus::WaitingForApproval,
    }) {
        deny_failed_approval(broker, events, &approval_id, "status_event_failed");
        return Err(ApprovalRequestError::Event(error));
    }
    if let Err(error) = events.emit(SessionEvent::ApprovalRequested {
        request: registered.request,
    }) {
        deny_failed_approval(broker, events, &approval_id, "request_event_failed");
        return Err(ApprovalRequestError::Event(error));
    }

    let mut receiver = registered.receiver;
    let broker_closed = || {
        let _ = events.emit(SessionEvent::TurnStatusChanged {
            status: TurnStatus::Running,
        });
        ApprovalRequestError::BrokerClosed
    };
    let resolution = if let Some(timeout) = broker.timeout {
        let mut ineligible_deadline = None;
        loop {
            let eligibility_changed = broker.eligibility_changed.notified();
            if broker.has_eligible_client(&approval_id) {
                tokio::select! {
                    resolution = &mut receiver => {
                        break resolution.map_err(|_| broker_closed())?;
                    }
                    _ = eligibility_changed => continue,
                }
            } else {
                let deadline = *ineligible_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + timeout);
                tokio::select! {
                    resolution = &mut receiver => {
                        break resolution.map_err(|_| broker_closed())?;
                    }
                    _ = eligibility_changed => continue,
                    _ = tokio::time::sleep_until(deadline) => {
                        match broker.resolve(
                            &approval_id,
                            "approval_timeout",
                            &[ClientCapability::ApproveOnce],
                            ApprovalDecision::Deny,
                        ) {
                            Ok(resolution) => break resolution,
                            Err(ApprovalError::NotPending) => continue,
                            Err(error) => {
                                return Err(ApprovalRequestError::Broker(error));
                            }
                        }
                    }
                }
            }
        }
    } else {
        receiver.await.map_err(|_| broker_closed())?
    };
    if let Some(unemitted) = broker.take_unemitted(&approval_id) {
        let _ = events.emit(SessionEvent::ApprovalResolved {
            approval_id: unemitted.approval_id,
            decision: unemitted.decision,
            resolved_by: unemitted.resolved_by,
        });
    }
    let _ = events.emit(SessionEvent::TurnStatusChanged {
        status: TurnStatus::Running,
    });
    Ok(resolution)
}

fn deny_failed_approval(
    broker: &ApprovalBroker,
    events: &SessionEventRouter,
    approval_id: &str,
    resolved_by: &str,
) {
    let _ = broker.resolve(
        approval_id,
        resolved_by,
        &[ClientCapability::ApproveOnce],
        ApprovalDecision::Deny,
    );
    let _ = broker.take_unemitted(approval_id);
    let _ = events.emit(SessionEvent::TurnStatusChanged {
        status: TurnStatus::Running,
    });
}

/// Frontend-neutral tool lifecycle shared by ACP, the local daemon client, and
/// future remote clients. Presentation adapters may add richer cards/diffs,
/// but canonical execution and exactly-one terminal event live here.
pub struct CanonicalToolLifecycle {
    events: Arc<SessionEventRouter>,
    approvals: Arc<ApprovalBroker>,
    safety: Arc<crate::safety::SafetyPolicy>,
    active: StdMutex<HashSet<String>>,
    max_active_tools: usize,
    max_output_bytes: Option<usize>,
}

impl CanonicalToolLifecycle {
    #[cfg(test)]
    pub fn new(
        events: Arc<SessionEventRouter>,
        approvals: Arc<ApprovalBroker>,
        safety: Arc<crate::safety::SafetyPolicy>,
        max_active_tools: usize,
    ) -> Self {
        Self {
            events,
            approvals,
            safety,
            active: StdMutex::new(HashSet::new()),
            max_active_tools: max_active_tools.max(1),
            max_output_bytes: None,
        }
    }

    pub fn new_with_output_limit(
        events: Arc<SessionEventRouter>,
        approvals: Arc<ApprovalBroker>,
        safety: Arc<crate::safety::SafetyPolicy>,
        max_active_tools: usize,
        max_output_bytes: usize,
    ) -> Self {
        Self {
            events,
            approvals,
            safety,
            active: StdMutex::new(HashSet::new()),
            max_active_tools: max_active_tools.max(1),
            max_output_bytes: Some(max_output_bytes.max(16)),
        }
    }

    pub async fn before(
        &self,
        info: &crate::agent::ToolCallInfo,
    ) -> crate::agent::BeforeHookResult {
        if let Err(reason) = self.reserve(info) {
            return crate::agent::BeforeHookResult::Block(reason);
        }
        self.authorize_reserved(info).await
    }

    pub(crate) fn approval_required(&self, tool: &str) -> bool {
        matches!(self.safety.gate(tool), crate::safety::Gate::NeedsApproval)
    }

    /// Atomically admit one tool call before any frontend announces it.
    /// Structural duplicate/capacity failures therefore stay absent from both
    /// canonical and adapter projections.
    pub fn reserve(&self, info: &crate::agent::ToolCallInfo) -> Result<(), String> {
        {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if active.contains(&info.id) {
                return Err(format!("tool call '{}' is already active", info.id));
            }
            if active.len() >= self.max_active_tools {
                return Err(format!(
                    "active tool-call limit ({}) reached",
                    self.max_active_tools
                ));
            }
            active.insert(info.id.clone());
        }

        let _ = self.events.emit(SessionEvent::ToolCallStarted {
            id: info.id.clone(),
            name: info.name.clone(),
            title: info.name.clone(),
            input_summary: None,
        });
        Ok(())
    }

    /// Apply policy/approval to a call already admitted by [`Self::reserve`].
    pub async fn authorize_reserved(
        &self,
        info: &crate::agent::ToolCallInfo,
    ) -> crate::agent::BeforeHookResult {
        let title = tool_call_title(info);
        let decision = match self.safety.gate(&info.name) {
            crate::safety::Gate::Block(reason) => crate::agent::BeforeHookResult::Block(reason),
            crate::safety::Gate::Allow => crate::agent::BeforeHookResult::Allow,
            crate::safety::Gate::NeedsApproval => {
                match request_approval(
                    &self.approvals,
                    &self.events,
                    ApprovalRequest::unassigned(info.id.clone(), info.name.clone(), title, true),
                )
                .await
                {
                    Ok(resolution) => match resolution.decision {
                        ApprovalDecision::AllowOnce => crate::agent::BeforeHookResult::Allow,
                        ApprovalDecision::AllowAlways => {
                            self.safety.remember_always(&info.name);
                            crate::agent::BeforeHookResult::Allow
                        }
                        ApprovalDecision::Deny => crate::agent::BeforeHookResult::Block(
                            approval_denial_reason(&info.name, &resolution),
                        ),
                    },
                    Err(_) => crate::agent::BeforeHookResult::Block(format!(
                        "permission broker unavailable for '{}'",
                        info.name
                    )),
                }
            }
        };

        let (status, blocked) = match &decision {
            crate::agent::BeforeHookResult::Allow => (
                crate::session_protocol::ToolCallStateStatus::InProgress,
                false,
            ),
            crate::agent::BeforeHookResult::Block(_) => {
                (crate::session_protocol::ToolCallStateStatus::Failed, true)
            }
        };
        let _ = self.events.emit(SessionEvent::ToolCallUpdated {
            id: info.id.clone(),
            status,
        });
        if blocked {
            self.finish(info, "blocked", true);
        }
        decision
    }

    /// Claim and emit one terminal completion. Returns false for a duplicate or
    /// late callback whose call was already completed/cancelled.
    pub fn finish(&self, info: &crate::agent::ToolCallInfo, output: &str, is_error: bool) -> bool {
        let was_active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&info.id);
        if !was_active {
            return false;
        }
        let output = self.project_output(output);
        let _ = self.events.emit(SessionEvent::ToolCallFinished {
            id: info.id.clone(),
            status: if is_error {
                crate::session_protocol::ToolCallStateStatus::Failed
            } else {
                crate::session_protocol::ToolCallStateStatus::Completed
            },
            output,
        });
        true
    }

    fn project_output(&self, output: &str) -> String {
        self.max_output_bytes.map_or_else(
            || output.to_string(),
            |max| bounded_tool_output(output, max),
        )
    }

    pub fn cancel_all(&self) {
        let mut ids: Vec<String> = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .collect();
        ids.sort();
        for id in &ids {
            let _ = self.events.emit(SessionEvent::ToolCallUpdated {
                id: id.clone(),
                status: crate::session_protocol::ToolCallStateStatus::Cancelled,
            });
            let _ = self.events.emit(SessionEvent::ToolCallFinished {
                id: id.clone(),
                status: crate::session_protocol::ToolCallStateStatus::Cancelled,
                output: "cancelled".to_string(),
            });
        }
    }

    fn apply_approval_side_effects(&self, resolution: &ApprovalResolution) {
        if resolution.decision == ApprovalDecision::AllowAlways {
            self.safety.remember_always(&resolution.tool);
        }
    }
}

fn approval_denial_reason(tool: &str, resolution: &ApprovalResolution) -> String {
    match resolution.resolved_by.as_str() {
        "acp_cancelled" => format!("permission request cancelled for '{tool}'"),
        "acp_unrecognized" => format!("unrecognized permission outcome for '{tool}'"),
        "acp_request_failed" | "acp_unavailable" | "acp_spawn_failed" => {
            format!("permission request failed for '{tool}'")
        }
        "broker_closed" => format!("permission broker closed for '{tool}'"),
        "resolution_failed" => format!("permission resolution failed for '{tool}'"),
        _ => format!("permission denied for '{tool}'"),
    }
}

fn bounded_tool_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    const MARKER: &str = "\n[tool output truncated]";
    let prefix_budget = max_bytes.saturating_sub(MARKER.len());
    let mut end = prefix_budget.min(output.len());
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = output[..end].to_string();
    if max_bytes >= MARKER.len() {
        bounded.push_str(MARKER);
    } else {
        bounded.push_str(&MARKER[..max_bytes]);
    }
    bounded
}

pub(crate) fn tool_call_title(info: &crate::agent::ToolCallInfo) -> String {
    if info.name == "exec" {
        info.input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| info.name.clone())
    } else {
        info.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_protocol::TurnStatus;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            id: String::new(),
            tool_call_id: "tool".to_string(),
            tool: "exec".to_string(),
            detail: "run tests".to_string(),
            allow_always_available: true,
        }
    }

    #[tokio::test]
    async fn turn_controller_rejects_overlap_and_routes_cancellation() {
        let controller = TurnController::default();
        let active = controller.begin().expect("first turn starts");
        assert!(controller.is_active());
        assert!(matches!(controller.begin(), Err(TurnError::Busy)));
        assert!(controller.cancel());
        tokio::time::timeout(std::time::Duration::from_millis(100), active.cancelled())
            .await
            .expect("active turn receives cancellation");
        drop(active);
        assert!(!controller.is_active());
    }

    #[tokio::test]
    async fn cancellation_signal_does_not_leak_into_the_next_turn() {
        let controller = TurnController::default();
        let first = controller.begin().expect("first turn starts");
        let first_signal = std::sync::Arc::clone(&first.signal);
        assert!(controller.cancel());
        first.cancelled().await;
        drop(first);

        let second = controller.begin().expect("second turn starts");
        assert!(!std::sync::Arc::ptr_eq(&first_signal, &second.signal));
        assert!(controller.cancel());
        second.cancelled().await;
    }

    #[test]
    fn dropping_active_turn_clears_slot_and_idle_cancel_is_safe() {
        let controller = TurnController::default();
        assert!(!controller.cancel());
        {
            let _active = controller.begin().expect("turn starts");
            assert!(controller.is_active());
        }
        assert!(!controller.is_active());
        assert!(controller.begin().is_ok());
    }

    #[tokio::test]
    async fn register_exposes_pending_and_resolution_wakes_waiter() {
        let broker = ApprovalBroker::new(false);
        let registered = broker.register(request()).unwrap();
        let approval_id = registered.request.id.clone();
        assert_eq!(broker.pending(), vec![registered.request.clone()]);
        assert!(!registered.request.allow_always_available);

        let resolution = broker
            .resolve(
                &approval_id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            )
            .unwrap();
        assert_eq!(registered.receiver.await.unwrap(), resolution);
        assert!(broker.pending().is_empty());
        assert_eq!(
            broker.resolve(
                &approval_id,
                "late",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            ),
            Err(ApprovalError::AlreadyResolved)
        );
        assert_eq!(
            broker.resolve(
                "approval-999",
                "unknown",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            ),
            Err(ApprovalError::NotPending)
        );
    }

    #[tokio::test]
    async fn resolved_but_unemitted_approval_is_claimed_by_cancellation_once() {
        let broker = ApprovalBroker::new(true);
        let registered = broker.register(request()).unwrap();
        let resolution = broker
            .resolve(
                &registered.request.id,
                "acp_local",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            )
            .unwrap();

        assert_eq!(
            broker.cancel_all("session_cancelled"),
            vec![resolution.clone()],
            "cancellation must recover a resolution whose waiter was dropped"
        );
        assert!(broker.cancel_all("session_cancelled").is_empty());
        assert_eq!(registered.receiver.await.unwrap(), resolution);
    }

    #[test]
    fn approve_always_only_client_is_eligible_for_available_request() {
        let broker = ApprovalBroker::new(true);
        let registered = broker.register(request()).unwrap();
        broker.set_eligible_client_counts(0, 1);
        assert!(broker.has_eligible_client(&registered.request.id));
        broker.set_eligible_client_counts(0, 0);
        assert!(!broker.has_eligible_client(&registered.request.id));
    }

    #[test]
    fn recovered_allow_always_resolution_preserves_safety_side_effect() {
        let safety = std::sync::Arc::new(crate::safety::SafetyPolicy {
            approval_mode: crate::safety::ApprovalMode::Interactive,
            ..crate::safety::SafetyPolicy::default()
        });
        assert!(matches!(
            safety.gate("exec"),
            crate::safety::Gate::NeedsApproval
        ));
        let lifecycle = CanonicalToolLifecycle::new(
            std::sync::Arc::new(SessionEventRouter::default()),
            std::sync::Arc::new(ApprovalBroker::new(true)),
            std::sync::Arc::clone(&safety),
            1,
        );
        lifecycle.apply_approval_side_effects(&ApprovalResolution {
            approval_id: "approval-1".to_string(),
            tool: "exec".to_string(),
            decision: ApprovalDecision::AllowAlways,
            resolved_by: "acp_local".to_string(),
        });
        assert!(matches!(safety.gate("exec"), crate::safety::Gate::Allow));
    }

    #[test]
    fn approval_denial_reason_preserves_transport_failures() {
        let reason = approval_denial_reason(
            "exec",
            &ApprovalResolution {
                approval_id: "approval-1".to_string(),
                tool: "exec".to_string(),
                decision: ApprovalDecision::Deny,
                resolved_by: "acp_request_failed".to_string(),
            },
        );
        assert_eq!(reason, "permission request failed for 'exec'");
        assert_eq!(
            approval_denial_reason(
                "exec",
                &ApprovalResolution {
                    approval_id: "approval-2".to_string(),
                    tool: "exec".to_string(),
                    decision: ApprovalDecision::Deny,
                    resolved_by: "acp_cancelled".to_string(),
                },
            ),
            "permission request cancelled for 'exec'"
        );
    }

    #[tokio::test]
    async fn canonical_approval_waits_for_transport_independent_broker_resolution() {
        let broker = std::sync::Arc::new(ApprovalBroker::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let broker_for_handler = std::sync::Arc::clone(&broker);
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let events = SessionEventRouter::new(Some(std::sync::Arc::new(move |_seq, event| {
            seen_for_handler
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event.clone());
            if let SessionEvent::ApprovalRequested { request } = event {
                broker_for_handler
                    .resolve(
                        &request.id,
                        "headless",
                        &[ClientCapability::ApproveOnce],
                        ApprovalDecision::AllowOnce,
                    )
                    .expect("headless client resolves approval");
            }
        })));

        let resolution = request_approval(&broker, &events, request())
            .await
            .expect("approval resolves");
        assert_eq!(resolution.decision, ApprovalDecision::AllowOnce);
        assert_eq!(resolution.resolved_by, "headless");
        assert!(broker.pending().is_empty());

        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(
            seen.as_slice(),
            [
                SessionEvent::TurnStatusChanged {
                    status: TurnStatus::WaitingForApproval
                },
                SessionEvent::ApprovalRequested { .. },
                SessionEvent::ApprovalResolved { .. },
                SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Running
                }
            ]
        ));
    }

    #[tokio::test]
    async fn canonical_approval_timeout_denies_and_emits_once() {
        let broker = ApprovalBroker::new_with_timeout(false, std::time::Duration::from_millis(10));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let events = SessionEventRouter::new(Some(std::sync::Arc::new(move |_seq, event| {
            seen_for_handler
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        })));

        let resolution = request_approval(&broker, &events, request())
            .await
            .expect("timeout resolves safely");

        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert_eq!(resolution.resolved_by, "approval_timeout");
        assert!(broker.pending().is_empty());
        assert_eq!(
            seen.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter(|event| matches!(event, SessionEvent::ApprovalResolved { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn canonical_approval_timeout_pauses_for_eligible_client() {
        let broker = std::sync::Arc::new(ApprovalBroker::new_with_timeout(
            false,
            std::time::Duration::from_millis(10),
        ));
        broker.set_eligible_client_counts(1, 0);
        let events = std::sync::Arc::new(SessionEventRouter::default());
        let request_broker = std::sync::Arc::clone(&broker);
        let request_events = std::sync::Arc::clone(&events);
        let pending = tokio::spawn(async move {
            request_approval(&request_broker, &request_events, request()).await
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let approval = broker.pending().pop().expect("approval remains pending");
        broker
            .resolve(
                &approval.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            )
            .unwrap();

        assert_eq!(
            pending.await.unwrap().unwrap().decision,
            ApprovalDecision::AllowOnce
        );
    }

    #[tokio::test]
    async fn approval_churn_does_not_extend_first_ineligible_deadline() {
        let broker = std::sync::Arc::new(ApprovalBroker::new_with_timeout(
            false,
            std::time::Duration::from_millis(30),
        ));
        let events = std::sync::Arc::new(SessionEventRouter::default());
        let request_broker = std::sync::Arc::clone(&broker);
        let request_events = std::sync::Arc::clone(&events);
        let pending = tokio::spawn(async move {
            request_approval(&request_broker, &request_events, request()).await
        });

        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        broker.set_eligible_client_counts(1, 0);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        broker.set_eligible_client_counts(0, 0);

        let resolution = tokio::time::timeout(std::time::Duration::from_millis(15), pending)
            .await
            .expect("expired original deadline denies immediately")
            .unwrap()
            .unwrap();
        assert_eq!(resolution.decision, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn canonical_tool_lifecycle_runs_without_any_frontend_connection() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let events = std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
            move |_seq, event| {
                seen_for_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        ))));
        let lifecycle = CanonicalToolLifecycle::new(
            std::sync::Arc::clone(&events),
            std::sync::Arc::new(ApprovalBroker::new(false)),
            std::sync::Arc::new(crate::safety::SafetyPolicy::default()),
            4,
        );
        let info = crate::agent::ToolCallInfo {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "README.md"}),
        };

        assert!(matches!(
            lifecycle.before(&info).await,
            crate::agent::BeforeHookResult::Allow
        ));
        assert!(lifecycle.finish(&info, "contents", false));
        assert!(!lifecycle.finish(&info, "duplicate", false));

        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(
            seen.as_slice(),
            [
                SessionEvent::ToolCallStarted { id, .. },
                SessionEvent::ToolCallUpdated {
                    id: updated_id,
                    status: crate::session_protocol::ToolCallStateStatus::InProgress,
                },
                SessionEvent::ToolCallFinished {
                    id: finished_id,
                    status: crate::session_protocol::ToolCallStateStatus::Completed,
                    output,
                }
            ] if id == "tool-1"
                && updated_id == "tool-1"
                && finished_id == "tool-1"
                && output == "contents"
        ));
    }

    #[tokio::test]
    async fn canonical_tool_output_is_utf8_safe_and_bounded_before_emit() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let events = std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
            move |_seq, event| {
                seen_for_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        ))));
        let lifecycle = CanonicalToolLifecycle::new_with_output_limit(
            events,
            std::sync::Arc::new(ApprovalBroker::new(false)),
            std::sync::Arc::new(crate::safety::SafetyPolicy::default()),
            1,
            64,
        );
        let info = crate::agent::ToolCallInfo {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "large"}),
        };
        assert!(matches!(
            lifecycle.before(&info).await,
            crate::agent::BeforeHookResult::Allow
        ));

        lifecycle.finish(&info, &"🙂\0".repeat(100), false);

        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let output = seen.iter().find_map(|event| match event {
            SessionEvent::ToolCallFinished { output, .. } => Some(output),
            _ => None,
        });
        let output = output.expect("terminal tool event");
        assert!(output.len() <= 64);
        assert!(output.contains("[tool output truncated]"));
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn canonical_tool_lifecycle_enforces_duplicate_and_capacity_bounds() {
        let events = std::sync::Arc::new(SessionEventRouter::default());
        let lifecycle = CanonicalToolLifecycle::new(
            events,
            std::sync::Arc::new(ApprovalBroker::new(false)),
            std::sync::Arc::new(crate::safety::SafetyPolicy::default()),
            1,
        );
        let first = crate::agent::ToolCallInfo {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "README.md"}),
        };
        let second = crate::agent::ToolCallInfo {
            id: "tool-2".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "AGENTS.md"}),
        };

        assert!(matches!(
            lifecycle.before(&first).await,
            crate::agent::BeforeHookResult::Allow
        ));
        assert!(matches!(
            lifecycle.before(&first).await,
            crate::agent::BeforeHookResult::Block(reason)
                if reason.contains("already active")
        ));
        assert!(matches!(
            lifecycle.before(&second).await,
            crate::agent::BeforeHookResult::Block(reason)
                if reason.contains("active tool-call limit")
        ));
        assert!(lifecycle.finish(&first, "done", false));
    }

    #[tokio::test]
    async fn canonical_exec_title_is_safe_while_approval_detail_is_specific() {
        let broker = std::sync::Arc::new(ApprovalBroker::new(false));
        let broker_for_handler = std::sync::Arc::clone(&broker);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let events = std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
            move |_seq, event| {
                if let SessionEvent::ApprovalRequested { request } = &event {
                    broker_for_handler
                        .resolve(
                            &request.id,
                            "test_client",
                            &[ClientCapability::ApproveOnce],
                            ApprovalDecision::Deny,
                        )
                        .unwrap();
                }
                seen_for_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        ))));
        let lifecycle = CanonicalToolLifecycle::new(
            events,
            broker,
            std::sync::Arc::new(crate::safety::SafetyPolicy {
                approval_mode: crate::safety::ApprovalMode::Interactive,
                ..crate::safety::SafetyPolicy::default()
            }),
            1,
        );
        let info = crate::agent::ToolCallInfo {
            id: "tool-1".to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"command": "printf sensitive-value"}),
        };

        assert!(matches!(
            lifecycle.before(&info).await,
            crate::agent::BeforeHookResult::Block(_)
        ));
        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(seen.iter().any(|event| matches!(
            event,
            SessionEvent::ToolCallStarted { title, .. } if title == "exec"
        )));
        assert!(seen.iter().any(|event| matches!(
            event,
            SessionEvent::ApprovalRequested { request }
                if request.detail == "printf sensitive-value"
        )));
    }

    #[test]
    fn canonical_reservation_rejects_before_emitting_a_second_start() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let lifecycle = CanonicalToolLifecycle::new(
            std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
                move |_seq, event| {
                    seen_for_handler
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(event);
                },
            )))),
            std::sync::Arc::new(ApprovalBroker::new(false)),
            std::sync::Arc::new(crate::safety::SafetyPolicy::default()),
            1,
        );
        let first = crate::agent::ToolCallInfo {
            id: "tool-1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "README.md"}),
        };
        let second = crate::agent::ToolCallInfo {
            id: "tool-2".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "AGENTS.md"}),
        };

        lifecycle.reserve(&first).expect("first reservation");
        assert!(lifecycle.reserve(&first).is_err(), "duplicate id must fail");
        assert!(lifecycle.reserve(&second).is_err(), "capacity must fail");

        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, SessionEvent::ToolCallStarted { .. }))
                .count(),
            1,
            "rejected reservations must not create canonical-only tool cards"
        );
    }

    #[tokio::test]
    async fn prompt_execution_is_transport_independent() {
        struct StaticProvider;

        #[async_trait::async_trait]
        impl crate::providers::LlmProvider for StaticProvider {
            async fn complete(
                &self,
                _context: &crate::providers::Context,
                _options: &crate::providers::CompleteOpts,
            ) -> crate::providers::LlmResponse {
                crate::providers::LlmResponse {
                    content: vec![crate::providers::ContentBlock::Text("pong".to_string())],
                    stop_reason: crate::providers::StopReason::EndTurn,
                    error_message: None,
                    context_overflow: false,
                    usage: crate::providers::Usage {
                        input: 4,
                        output: 2,
                        ..crate::providers::Usage::default()
                    },
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let config = std::sync::Arc::new(crate::config::Config::default());
        let tool_session = crate::session::Session::new(dir.path().to_path_buf(), config);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let events = std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
            move |_sequence, event| {
                seen_for_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            },
        ))));
        let approvals = std::sync::Arc::new(ApprovalBroker::new(false));
        let lifecycle = std::sync::Arc::new(CanonicalToolLifecycle::new(
            std::sync::Arc::clone(&events),
            std::sync::Arc::clone(&approvals),
            std::sync::Arc::new(crate::safety::SafetyPolicy::default()),
            4,
        ));
        let core = SessionCore::new(
            crate::agent::AgentSession::new(
                Box::new(StaticProvider),
                tool_session,
                crate::agent::AgentConfig {
                    opts: crate::providers::CompleteOpts {
                        model: "test-model".to_string(),
                        ..crate::providers::CompleteOpts::default()
                    },
                    ..crate::agent::AgentConfig::default()
                },
            ),
            "test-model".to_string(),
            dir.path().to_path_buf(),
            SessionCompaction::new(None, false),
            HashMap::new(),
            approvals,
            None,
            std::sync::Arc::clone(&events),
            lifecycle,
        );
        let message = crate::providers::Message {
            role: crate::providers::Role::User,
            content: vec![crate::providers::ContentBlock::Text("ping".to_string())],
        };

        let execution = core
            .prompt(
                message,
                "ping".to_string(),
                Some("request-1".to_string()),
                None,
                || {},
                |_| crate::session_protocol::AssistantOutcome::Completed,
            )
            .await
            .expect("prompt executes without a frontend connection");
        assert!(matches!(
            execution.outcome,
            SessionPromptOutcome::Completed(_)
        ));
        let history = core.session.lock().await.history().to_vec();
        assert!(history.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, crate::providers::ContentBlock::Text(text) if text == "pong"),
            )
        }));
        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(seen.iter().any(|event| matches!(
            event,
            SessionEvent::UserMessage {
                text,
                request_id: Some(request_id),
            } if text == "ping" && request_id == "request-1"
        )));
        assert!(seen.iter().any(|event| matches!(
            event,
            SessionEvent::AssistantDone {
                outcome: crate::session_protocol::AssistantOutcome::Completed
            }
        )));
    }

    #[test]
    fn observer_cannot_answer_privileged_approval() {
        let broker = ApprovalBroker::new(false);
        let registered = broker.register(request()).unwrap();
        assert_eq!(
            broker.resolve(
                &registered.request.id,
                "observer",
                &[ClientCapability::Observe],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::MissingCapability(
                ClientCapability::ApproveOnce
            ))
        );
        assert_eq!(broker.pending(), vec![registered.request]);
    }

    #[test]
    fn allow_always_requires_host_policy_request_option_and_capability() {
        let disabled = ApprovalBroker::new(false);
        let registered = disabled.register(request()).unwrap();
        assert_eq!(
            disabled.resolve(
                &registered.request.id,
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::AllowAlwaysUnavailable)
        );

        let enabled = ApprovalBroker::new(true);
        let mut unavailable = request();
        unavailable.allow_always_available = false;
        let unavailable = enabled.register(unavailable).unwrap();
        assert_eq!(
            enabled.resolve(
                &unavailable.request.id,
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::AllowAlwaysUnavailable)
        );

        let available = enabled.register(request()).unwrap();
        assert_eq!(
            enabled.resolve(
                &available.request.id,
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::MissingCapability(
                ClientCapability::ApproveAlways
            ))
        );
        assert!(enabled
            .resolve(
                &available.request.id,
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            )
            .is_ok());
    }

    #[test]
    fn first_resolution_wins_and_late_answers_are_classified() {
        let broker = ApprovalBroker::new(false);
        let registered = broker.register(request()).unwrap();
        broker
            .resolve(
                &registered.request.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            )
            .unwrap();
        assert_eq!(
            broker.resolve(
                &registered.request.id,
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            ),
            Err(ApprovalError::AlreadyResolved)
        );
        assert_eq!(
            broker.resolve(
                "never-issued",
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::NotPending)
        );
    }

    #[test]
    fn broker_assigns_monotonic_ids_that_are_never_reused() {
        let broker = ApprovalBroker::new(false);
        let first = broker.register(request()).unwrap();
        let second = broker.register(request()).unwrap();
        assert_eq!(first.request.id, "approval-1");
        assert_eq!(second.request.id, "approval-2");
        broker
            .resolve(
                &first.request.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            )
            .unwrap();
        let third = broker.register(request()).unwrap();
        assert_eq!(third.request.id, "approval-3");
    }

    #[tokio::test]
    async fn cancel_all_denies_every_pending_waiter() {
        let broker = ApprovalBroker::new(false);
        let first = broker.register(request()).unwrap();
        let second = broker.register(request()).unwrap();
        let resolutions = broker.cancel_all("daemon_shutdown");
        assert_eq!(resolutions.len(), 2);
        assert!(broker.pending().is_empty());
        assert_eq!(
            first.receiver.await.unwrap().decision,
            ApprovalDecision::Deny
        );
        assert_eq!(
            second.receiver.await.unwrap().decision,
            ApprovalDecision::Deny
        );
    }

    #[test]
    fn cancel_all_preserves_numeric_approval_order() {
        let broker = ApprovalBroker::new(false);
        for index in 1..=12 {
            let mut request = request();
            request.tool_call_id = format!("tool-{index}");
            broker.register(request).unwrap();
        }
        let ids = broker
            .cancel_all("session_cancelled")
            .into_iter()
            .map(|resolution| resolution.approval_id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            (1..=12)
                .map(|index| format!("approval-{index}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn dropping_broker_structurally_denies_pending_waiters() {
        let registered = {
            let broker = ApprovalBroker::new(false);
            broker.register(request()).unwrap()
        };
        let resolution = registered.receiver.await.unwrap();
        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert_eq!(resolution.resolved_by, "broker_drop");
    }

    /// Collect every `TurnStatusChanged` status a router sees, in order.
    fn turn_status_recorder() -> (
        SessionEventRouter,
        std::sync::Arc<std::sync::Mutex<Vec<TurnStatus>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_hook = std::sync::Arc::clone(&seen);
        let router = SessionEventRouter::new(Some(std::sync::Arc::new(move |_seq, event| {
            if let SessionEvent::TurnStatusChanged { status } = event {
                seen_for_hook
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(status);
            }
        })));
        (router, seen)
    }

    /// A cancelled turn must be distinguishable from a completed one in the
    /// canonical stream, and the terminal event must come last.
    ///
    /// Asserting the *ordered sequence* matters. "a terminal event was emitted"
    /// or "the stream contains Cancelling" both held before this fix, when every
    /// turn ended on `Idle` and `Cancelling` could arrive after it.
    #[tokio::test]
    async fn cancelled_turn_ends_on_cancelled_after_cancelling() {
        let (router, seen) = turn_status_recorder();
        let controller = TurnController::default();

        {
            let active = controller.begin().expect("turn starts");
            let _ = router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            });
            let turn = SessionTurn {
                active,
                events: &router,
            };
            // Mirrors SessionCore::cancel_turn: mark cancelled, emit Cancelling,
            // then signal — so Cancelling precedes the unwind it causes.
            assert!(controller.cancel_with(|| {
                let _ = router.emit(SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Cancelling,
                });
            }));
            tokio::time::timeout(std::time::Duration::from_millis(100), turn.cancelled())
                .await
                .expect("turn observes cancellation");
        }

        let statuses = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            statuses,
            vec![
                TurnStatus::Running,
                TurnStatus::Cancelling,
                TurnStatus::Cancelled
            ],
        );
    }

    /// A cancel that loses the outcome claim must not relabel a finished turn.
    /// Before the claim existed, a cancel landing between prompt completion and
    /// `SessionTurn` drop produced a canonical `Cancelled` for a turn whose ACP
    /// response was `EndTurn`.
    #[tokio::test]
    async fn cancel_after_completion_is_refused_and_turn_ends_idle() {
        let (router, seen) = turn_status_recorder();
        let controller = TurnController::default();

        {
            let active = controller.begin().expect("turn starts");
            let _ = router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            });
            let turn = SessionTurn {
                active,
                events: &router,
            };
            turn.mark_completed();
            // The cancel arrives during post-completion bookkeeping: it must be
            // refused, and must not emit Cancelling.
            assert!(!controller.cancel_with(|| {
                let _ = router.emit(SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Cancelling,
                });
            }));
        }

        let statuses = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(statuses, vec![TurnStatus::Running, TurnStatus::Idle]);
    }

    /// A repeated cancel re-notifies but must not win a second `Cancelling`
    /// emission — the callback runs only on the Live -> Cancelled transition.
    #[tokio::test]
    async fn repeated_cancel_emits_cancelling_once() {
        let (router, seen) = turn_status_recorder();
        let controller = TurnController::default();

        {
            let active = controller.begin().expect("turn starts");
            let turn = SessionTurn {
                active,
                events: &router,
            };
            let cancelling = || {
                let _ = router.emit(SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Cancelling,
                });
            };
            assert!(controller.cancel_with(cancelling));
            assert!(controller.cancel_with(cancelling));
            tokio::time::timeout(std::time::Duration::from_millis(100), turn.cancelled())
                .await
                .expect("turn observes cancellation");
        }

        let statuses = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            statuses,
            vec![TurnStatus::Cancelling, TurnStatus::Cancelled],
            "exactly one Cancelling for two cancel calls"
        );
    }

    #[tokio::test]
    async fn completed_turn_ends_on_idle() {
        let (router, seen) = turn_status_recorder();
        let controller = TurnController::default();

        {
            let active = controller.begin().expect("turn starts");
            let _ = router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            });
            let _turn = SessionTurn {
                active,
                events: &router,
            };
        }

        let statuses = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(statuses, vec![TurnStatus::Running, TurnStatus::Idle]);
    }

    #[test]
    fn completed_error_turn_has_one_fully_ordered_canonical_sequence() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_hook = std::sync::Arc::clone(&seen);
        let router = SessionEventRouter::new(Some(std::sync::Arc::new(move |_seq, event| {
            seen_for_hook
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        })));
        let controller = TurnController::default();
        {
            let active = controller.begin().expect("turn starts");
            let _ = router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            });
            let turn = SessionTurn {
                active,
                events: &router,
            };
            let _ = router.emit(SessionEvent::AssistantDelta {
                text: "partial".to_string(),
            });
            let _ = router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::WaitingForApproval,
            });
            let _ = router.emit(SessionEvent::ApprovalRequested { request: request() });
            let _ = router.emit(SessionEvent::ApprovalResolved {
                approval_id: "approval-1".to_string(),
                decision: ApprovalDecision::Deny,
                resolved_by: "acp_local".to_string(),
            });
            let _ = router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            });
            turn.mark_completed();
            let _ = router.emit(SessionEvent::ContextUsageChanged {
                usage: crate::session_protocol::ContextUsage::new(10, Some(100), 10, Some(72)),
            });
            let _ = router.emit(SessionEvent::AssistantDone {
                outcome: crate::session_protocol::AssistantOutcome::Errored {
                    context_overflow: true,
                    message: "context exceeded".to_string(),
                },
            });
        }

        assert_eq!(
            *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![
                SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Running,
                },
                SessionEvent::AssistantDelta {
                    text: "partial".to_string(),
                },
                SessionEvent::TurnStatusChanged {
                    status: TurnStatus::WaitingForApproval,
                },
                SessionEvent::ApprovalRequested { request: request() },
                SessionEvent::ApprovalResolved {
                    approval_id: "approval-1".to_string(),
                    decision: ApprovalDecision::Deny,
                    resolved_by: "acp_local".to_string(),
                },
                SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Running,
                },
                SessionEvent::ContextUsageChanged {
                    usage: crate::session_protocol::ContextUsage::new(10, Some(100), 10, Some(72),),
                },
                SessionEvent::AssistantDone {
                    outcome: crate::session_protocol::AssistantOutcome::Errored {
                        context_overflow: true,
                        message: "context exceeded".to_string(),
                    },
                },
                SessionEvent::TurnStatusChanged {
                    status: TurnStatus::Idle,
                },
            ]
        );
    }

    #[test]
    fn every_session_event_variant_has_a_production_emission_site() {
        let core = include_str!("session_core.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        let acp = include_str!("acp_cmd.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        let production: String = format!("{core}\n{acp}")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        for (variant, marker) in [
            ("UserMessage", "emit(SessionEvent::UserMessage"),
            ("AssistantDelta", "emit(SessionEvent::AssistantDelta"),
            ("AssistantDone", "emit(CoreSessionEvent::AssistantDone"),
            ("ThoughtDelta", "emit(CoreSessionEvent::ThoughtDelta"),
            ("ToolCallStarted", "emit(SessionEvent::ToolCallStarted"),
            ("ToolCallUpdated", "emit(SessionEvent::ToolCallUpdated"),
            ("ToolCallFinished", "emit(SessionEvent::ToolCallFinished"),
            ("ApprovalRequested", "emit(SessionEvent::ApprovalRequested"),
            ("ApprovalResolved", "emit(SessionEvent::ApprovalResolved"),
            (
                "RuntimeOptionsChanged",
                "emit(CoreSessionEvent::RuntimeOptionsChanged",
            ),
            (
                "ContextUsageChanged",
                "emit(CoreSessionEvent::ContextUsageChanged",
            ),
            ("TurnStatusChanged", "emit(SessionEvent::TurnStatusChanged"),
            ("SessionEnding", "emit(CoreSessionEvent::SessionEnding"),
        ] {
            assert!(
                production.contains(marker),
                "SessionEvent::{variant} has no production emission site ({marker})"
            );
        }
    }

    #[test]
    fn canonical_event_sites_cover_both_turn_and_session_lifecycle_paths() {
        let source = include_str!("acp_cmd.rs");
        let prompt = source
            .split("async fn run_prompt_turn")
            .nth(1)
            .unwrap()
            .split("async fn run_retry_turn")
            .next()
            .unwrap();
        let retry = source
            .split("async fn run_retry_turn")
            .nth(1)
            .unwrap()
            .split("async fn truncate_session")
            .next()
            .unwrap();
        assert!(prompt.contains(".prompt("));
        assert!(retry.contains("emit_assistant_done"));
        assert!(retry.contains("cleanup_cancelled_turn"));
        let core_source = include_str!("session_core.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(core_source.contains("emit(SessionEvent::AssistantDone"));
        assert!(core_source.contains("self.tool_lifecycle.cancel_all()"));

        let permission = core_source
            .split("pub async fn request_approval")
            .nth(1)
            .unwrap()
            .split("fn deny_failed_approval")
            .next()
            .unwrap();
        assert!(permission.contains("TurnStatus::WaitingForApproval"));
        assert!(permission.contains("TurnStatus::Running"));

        let delete = source
            .split("move |req: DeleteSessionRequest")
            .nth(1)
            .unwrap()
            .split("move |req: NewSessionRequest")
            .next()
            .unwrap();
        assert!(delete.contains("SESSION_END_REASON_DELETED"));
        let shutdown = source
            .split("async fn shutdown_all_bridges")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(shutdown.contains("SESSION_END_REASON_ENGINE_SHUTDOWN"));
    }

    #[test]
    fn event_router_assigns_monotonic_sequences_and_forwards_events() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_hook = std::sync::Arc::clone(&seen);
        let router = SessionEventRouter::new(Some(std::sync::Arc::new(move |seq, event| {
            seen_for_hook
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((seq, event));
        })));

        assert_eq!(
            router.emit(SessionEvent::TurnStatusChanged {
                status: TurnStatus::Running,
            }),
            Ok(1)
        );
        assert_eq!(
            router.emit(SessionEvent::AssistantDelta {
                text: "hello".to_string(),
            }),
            Ok(2)
        );
        assert_eq!(router.latest_sequence(), 2);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn restored_duplicate_tool_ids_pair_results_in_occurrence_order() {
        let mut calls = vec![
            crate::session_protocol::ToolCallState {
                id: "duplicate".to_string(),
                name: "first".to_string(),
                title: "first".to_string(),
                status: crate::session_protocol::ToolCallStateStatus::InProgress,
                output: None,
            },
            crate::session_protocol::ToolCallState {
                id: "duplicate".to_string(),
                name: "second".to_string(),
                title: "second".to_string(),
                status: crate::session_protocol::ToolCallStateStatus::InProgress,
                output: None,
            },
        ];
        apply_restored_tool_result(
            &mut calls,
            "duplicate",
            crate::session_protocol::ToolCallStateStatus::Completed,
            "first output".to_string(),
        );
        apply_restored_tool_result(
            &mut calls,
            "duplicate",
            crate::session_protocol::ToolCallStateStatus::Completed,
            "second output".to_string(),
        );
        assert_eq!(calls[0].output.as_deref(), Some("first output"));
        assert_eq!(calls[1].output.as_deref(), Some("second output"));
    }

    #[test]
    fn event_router_contains_adapter_panics() {
        let router = SessionEventRouter::new(Some(std::sync::Arc::new(|_, _| {
            panic!("adapter failed");
        })));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            router.emit(SessionEvent::AssistantDone {
                outcome: crate::session_protocol::AssistantOutcome::Completed,
            })
        }));
        assert_eq!(result.unwrap(), Err(SessionEventError::HandlerPanicked));
        assert_eq!(router.latest_sequence(), 1);
    }

    #[test]
    fn event_router_subscription_stops_delivery_when_dropped() {
        let router = std::sync::Arc::new(SessionEventRouter::default());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let subscription = router
            .subscribe(
                1,
                std::sync::Arc::new(move |seq, event| {
                    seen_for_handler
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((seq, event));
                }),
            )
            .expect("first bounded subscriber");

        router
            .emit(SessionEvent::AssistantDelta {
                text: "first".to_string(),
            })
            .unwrap();
        drop(subscription);
        router
            .emit(SessionEvent::AssistantDelta {
                text: "second".to_string(),
            })
            .unwrap();

        let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, 1);
    }

    #[test]
    fn event_router_publishes_concurrent_events_in_sequence_order() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_handler = std::sync::Arc::clone(&seen);
        let router = std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
            move |seq, _event| {
                if seq == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                seen_for_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(seq);
            },
        ))));
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for text in ["first", "second"] {
            let router = std::sync::Arc::clone(&router);
            let start = std::sync::Arc::clone(&start);
            threads.push(std::thread::spawn(move || {
                start.wait();
                router
                    .emit(SessionEvent::AssistantDelta {
                        text: text.to_string(),
                    })
                    .unwrap();
            }));
        }
        start.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(
            *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![1, 2]
        );
    }

    #[test]
    fn event_router_replays_retained_suffix_and_marks_evicted_gaps() {
        let router = SessionEventRouter::new_with_replay(None, 2);
        for text in ["one", "two", "three"] {
            router
                .emit(SessionEvent::AssistantDelta {
                    text: text.to_string(),
                })
                .unwrap();
        }

        assert!(matches!(
            router.replay_since(1),
            SessionReplay::Available {
                events,
                latest_seq: 3,
            } if events.iter().map(|(seq, _)| *seq).collect::<Vec<_>>() == vec![2, 3]
        ));
        assert_eq!(
            router.replay_since(0),
            SessionReplay::SnapshotRequired { latest_seq: 3 }
        );
        assert_eq!(
            router.replay_since(3),
            SessionReplay::Available {
                events: Vec::new(),
                latest_seq: 3,
            }
        );
        assert_eq!(
            router.replay_since(4),
            SessionReplay::SnapshotRequired { latest_seq: 3 }
        );
    }

    #[test]
    fn subscribe_and_capture_waits_for_in_flight_dispatch() {
        let started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let state = std::sync::Arc::new(std::sync::Mutex::new(0_u64));
        let handler_started = std::sync::Arc::clone(&started);
        let handler_release = std::sync::Arc::clone(&release);
        let handler_state = std::sync::Arc::clone(&state);
        let router = std::sync::Arc::new(SessionEventRouter::new(Some(std::sync::Arc::new(
            move |seq, _event| {
                handler_started.wait();
                handler_release.wait();
                *handler_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = seq;
            },
        ))));
        let emitter = {
            let router = std::sync::Arc::clone(&router);
            std::thread::spawn(move || {
                router
                    .emit(SessionEvent::AssistantDelta {
                        text: "event".to_string(),
                    })
                    .unwrap();
            })
        };
        started.wait();
        let capture_router = std::sync::Arc::clone(&router);
        let capture_state = std::sync::Arc::clone(&state);
        let capture = std::thread::spawn(move || {
            capture_router
                .subscribe_and_capture(1, std::sync::Arc::new(|_, _| {}), || {
                    *capture_state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                })
                .unwrap()
                .1
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!capture.is_finished());
        release.wait();

        emitter.join().unwrap();
        assert_eq!(capture.join().unwrap(), 1);
    }

    #[test]
    fn session_compaction_updates_only_provider_following_windows() {
        let policy = CompactionPolicy {
            high_water: 0.8,
            low_water: 0.5,
            context_window: 100,
            output_reservation: 10,
            summary_model: None,
            summary_prompt: None,
        };
        let dynamic = SessionCompaction::new(Some(policy.clone()), true);
        assert_eq!(
            dynamic
                .policy_for("model", Some(200))
                .unwrap()
                .unwrap()
                .context_window,
            200
        );
        let fixed = SessionCompaction::new(Some(policy), false);
        assert_eq!(
            fixed
                .policy_for("model", Some(200))
                .unwrap()
                .unwrap()
                .context_window,
            100
        );
    }
}
