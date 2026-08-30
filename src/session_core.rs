use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::oneshot;

use crate::agent::AgentSession;
use crate::compaction::CompactionPolicy;
use crate::providers::{Message, ThinkingLevel};
use crate::session_protocol::{
    ApprovalDecision, ApprovalRequest, AssistantOutcome, ClientCapability, ContextUsage,
    RuntimeOption, RuntimeValue, SessionEvent, SessionUsage, TurnStatus,
};
use crate::session_store::{SessionStore, SessionWriteOutcome, SessionWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnError {
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryMutationError {
    Busy,
}

impl std::fmt::Display for HistoryMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("session is busy")
    }
}

impl std::error::Error for HistoryMutationError {}

struct SessionMutationPermit<'a>(&'a AtomicBool);

impl<'a> SessionMutationPermit<'a> {
    fn acquire(active: &'a AtomicBool) -> Result<Self, HistoryMutationError> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self(active))
            .map_err(|_| HistoryMutationError::Busy)
    }
}

impl Drop for SessionMutationPermit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("session is busy"),
        }
    }
}

impl std::error::Error for TurnError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeConfigError {
    Busy,
    UnknownOption,
    InvalidValue,
    UnsupportedOption,
    ApplyFailed(String),
}

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
    writer: SessionWriter,
    store: SessionStore,
    state: Arc<StdMutex<SessionPersistenceState>>,
    catalog_writer: Option<Arc<crate::session_catalog::SessionCatalogWriter>>,
    retry_policy: PersistenceRetryPolicy,
    #[cfg(test)]
    save_failures: Arc<StdMutex<VecDeque<std::io::ErrorKind>>>,
    #[cfg(test)]
    save_pause: Arc<StdMutex<Option<TestSavePause>>>,
}

#[cfg(test)]
type TestSavePause = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Debug, Clone, Copy)]
pub struct PersistenceRetryPolicy {
    pub attempts: usize,
    pub initial_backoff: std::time::Duration,
    pub max_backoff: std::time::Duration,
}

impl PersistenceRetryPolicy {
    pub fn new(
        attempts: usize,
        initial_backoff: std::time::Duration,
        max_backoff: std::time::Duration,
    ) -> Self {
        Self {
            attempts: attempts.max(1),
            initial_backoff,
            max_backoff: max_backoff.max(initial_backoff),
        }
    }

    #[cfg(test)]
    pub(crate) fn single_attempt() -> Self {
        Self::new(1, std::time::Duration::ZERO, std::time::Duration::ZERO)
    }
}

#[derive(Debug, Clone)]
struct PersistenceCapture {
    /// Core-local capture order used exclusively for latest-wins admission.
    generation: u64,
    /// Core-local event coverage watermark. It may understate captured state
    /// and never participates in latest-wins admission.
    through_seq: u64,
    model: String,
    thinking: String,
    messages: Vec<Message>,
    cwd: PathBuf,
    client_user_message_ids: Vec<String>,
    assistant_outcomes: Vec<AssistantOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistenceSaveOutcome {
    Saved,
    SkippedDeleted,
    Superseded,
    SkippedStale { stored_generation: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistenceHealth {
    Clean,
    Dirty,
    Degraded { retryable: bool },
    Superseded,
}

struct SessionPersistenceState {
    deleted: bool,
    superseded: bool,
    /// Valid only for this SessionPersistence/core lifetime and never persisted.
    last_saved_generation: Option<u64>,
    health: PersistenceHealth,
}

impl Default for SessionPersistenceState {
    fn default() -> Self {
        Self {
            deleted: false,
            superseded: false,
            last_saved_generation: None,
            health: PersistenceHealth::Clean,
        }
    }
}

fn is_retryable_persistence_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    )
}

impl SessionPersistence {
    #[cfg(test)]
    pub fn new(
        session_id: impl Into<String>,
        store: SessionStore,
        retry_policy: PersistenceRetryPolicy,
    ) -> Self {
        let session_id = session_id.into();
        let expected_generation = store.load(&session_id).map(|record| record.generation);
        Self::claim(session_id, store, expected_generation, retry_policy).unwrap()
    }

    pub fn claim(
        session_id: impl Into<String>,
        store: SessionStore,
        expected_generation: Option<u64>,
        retry_policy: PersistenceRetryPolicy,
    ) -> std::io::Result<Self> {
        let session_id = session_id.into();
        let writer = store
            .claim_writer(&session_id, expected_generation)
            .map_err(std::io::Error::other)?;
        Ok(Self::with_writer(session_id, store, writer, retry_policy))
    }

    pub fn with_writer(
        session_id: String,
        store: SessionStore,
        writer: SessionWriter,
        retry_policy: PersistenceRetryPolicy,
    ) -> Self {
        Self {
            session_id,
            writer,
            store,
            state: Arc::new(StdMutex::new(SessionPersistenceState::default())),
            catalog_writer: None,
            retry_policy,
            #[cfg(test)]
            save_failures: Arc::new(StdMutex::new(VecDeque::new())),
            #[cfg(test)]
            save_pause: Arc::new(StdMutex::new(None)),
        }
    }

    pub fn with_catalog_writer(
        mut self,
        catalog_writer: Arc<crate::session_catalog::SessionCatalogWriter>,
    ) -> Self {
        self.catalog_writer = Some(catalog_writer);
        self
    }

    fn mark_dirty(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.deleted && !state.superseded {
            state.health = PersistenceHealth::Dirty;
        }
    }

    fn mark_clean_if_fully_handled(&self, handled_request: u64, requested: &AtomicU64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.deleted || state.superseded {
            return true;
        }
        if requested.load(Ordering::Acquire) <= handled_request {
            state.health = PersistenceHealth::Clean;
            true
        } else {
            state.health = PersistenceHealth::Dirty;
            false
        }
    }

    fn mark_degraded(&self, retryable: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.deleted && !state.superseded {
            state.health = PersistenceHealth::Degraded { retryable };
        }
    }

    #[allow(dead_code)] // Consumed by task 1409's daemon lifecycle policy.
    fn health(&self) -> PersistenceHealth {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.deleted {
            PersistenceHealth::Clean
        } else if state.superseded {
            PersistenceHealth::Superseded
        } else {
            state.health
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_saves(&self, failures: impl IntoIterator<Item = std::io::ErrorKind>) {
        self.save_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(failures);
    }

    #[cfg(test)]
    pub(crate) fn pause_next_save(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        *self
            .save_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((started_tx, release_rx));
        (started_rx, release_tx)
    }

    fn save(&self, capture: &PersistenceCapture) -> std::io::Result<PersistenceSaveOutcome> {
        // The state lock intentionally spans check + blocking write + watermark
        // update. This makes stale admission atomic and serializes delete;
        // callers release all SessionCore capture locks before entering here.
        // Production retries must recapture current state rather than reuse a
        // failed capture; task 1406 owns that retry loop.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.deleted {
            return Ok(PersistenceSaveOutcome::SkippedDeleted);
        }
        if state.superseded {
            return Ok(PersistenceSaveOutcome::Superseded);
        }
        if state
            .last_saved_generation
            .is_some_and(|last_saved_generation| capture.generation < last_saved_generation)
        {
            tracing::debug!(
                target: "daimonos::session_core",
                event = "session_payload_stale_capture_skipped",
                generation = capture.generation,
                through_seq = capture.through_seq,
                last_saved_generation = ?state.last_saved_generation,
            );
            return Ok(PersistenceSaveOutcome::SkippedStale {
                stored_generation: state.last_saved_generation.unwrap_or_default(),
            });
        }
        #[cfg(test)]
        if let Some((started, release)) = self
            .save_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = started.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(5));
        }
        let max_preview_bytes = self
            .catalog_writer
            .as_ref()
            .map(|writer| writer.max_preview_bytes())
            .unwrap_or(usize::MAX);
        #[cfg(test)]
        if let Some(kind) = self
            .save_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
        {
            return Err(std::io::Error::from(kind));
        }
        let write = self.store.save_acp_generation_result(
            &self.writer,
            capture.generation,
            &capture.model,
            &capture.thinking,
            &capture.messages,
            &capture.cwd,
            &capture.client_user_message_ids,
            &capture.assistant_outcomes,
            max_preview_bytes,
        )?;
        let write = match write {
            SessionWriteOutcome::Saved(write) => write,
            SessionWriteOutcome::Stale { stored_generation } => {
                state.last_saved_generation = Some(stored_generation);
                return Ok(PersistenceSaveOutcome::SkippedStale { stored_generation });
            }
            SessionWriteOutcome::Superseded => {
                state.superseded = true;
                return Ok(PersistenceSaveOutcome::Superseded);
            }
        };
        state.last_saved_generation = Some(capture.generation);
        if let Some(writer) = &self.catalog_writer {
            writer.enqueue_saved(write);
        }
        Ok(PersistenceSaveOutcome::Saved)
    }

    fn delete(&self) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.deleted || state.superseded {
            return Ok(false);
        }
        let deleted = match self.store.delete_writer(&self.writer) {
            Ok(Some(deleted)) => deleted,
            Ok(None) => {
                state.deleted = true;
                return Ok(false);
            }
            Err(error) => {
                tracing::warn!(
                    target: "daimonos::session_core",
                    event = "session_payload_delete_failed",
                    session_id = %self.session_id,
                    error = %error,
                    "persisted session deletion failed before tombstone commit"
                );
                return Err(error);
            }
        };
        state.deleted = true;
        if let Some(writer) = &self.catalog_writer {
            writer.enqueue_deleted(&self.session_id);
        }
        Ok(deleted)
    }
}

/// Daemon-owned, transport-independent state for one live agent session.
///
/// Provider, tools, safety hooks, history, usage, and runtime config live inside
/// `AgentSession`; this core adds lifecycle serialization, active-turn routing,
/// approvals, persistence identity, model/window state, and canonical events.
pub struct SessionCore {
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
    mutation_active: AtomicBool,
    pub(crate) session: tokio::sync::Mutex<AgentSession>,
    pub(crate) turn: TurnController,
    pub(crate) approvals: Arc<ApprovalBroker>,
    pub(crate) current_model: StdMutex<String>,
    current_thinking: StdMutex<ThinkingLevel>,
    pub(crate) cwd: PathBuf,
    pub(crate) client_user_message_ids: tokio::sync::Mutex<Vec<String>>,
    pub(crate) assistant_outcomes: StdMutex<Vec<AssistantOutcome>>,
    pub(crate) compaction: SessionCompaction,
    pub(crate) context_windows: tokio::sync::Mutex<HashMap<String, u64>>,
    pub(crate) events: Arc<SessionEventRouter>,
    pub(crate) tool_lifecycle: Arc<CanonicalToolLifecycle>,
    runtime_options: StdMutex<Vec<RuntimeOption>>,
    current_context_usage: StdMutex<Option<ContextUsage>>,
    persistence_generation: AtomicU64,
    persistence_gate: tokio::sync::Mutex<()>,
    persistence_requested: AtomicU64,
    persistence_completed: AtomicU64,
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
        // Mutation/turn exclusion is a two-claim protocol: clear publishes its
        // AcqRel permit before inspecting the turn slot; a turn checks that
        // permit both before and after claiming the slot. If the claims cross,
        // one side observes the other with Acquire ordering and releases its
        // own claim, so history mutation and prompt execution never overlap.
        if self.mutation_active.load(Ordering::Acquire) {
            return Err(TurnError::Busy);
        }
        let active = self.turn.begin()?;
        if self.mutation_active.load(Ordering::Acquire) {
            drop(active);
            return Err(TurnError::Busy);
        }
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

    pub fn turn_is_active(&self) -> bool {
        self.turn.is_active()
    }

    #[cfg(test)]
    pub fn mutation_is_active(&self) -> bool {
        self.mutation_active.load(Ordering::Acquire)
    }

    /// Reset canonical conversation history while retaining cumulative usage
    /// and runtime configuration. The clear event is sequenced so every
    /// attached/replaying frontend resets at the same point.
    pub async fn clear_history(&self) -> Result<bool, HistoryMutationError> {
        let _mutation = SessionMutationPermit::acquire(&self.mutation_active)?;
        if self.turn.is_active() {
            return Err(HistoryMutationError::Busy);
        }
        let _lifecycle = self.lifecycle.lock().await;
        if self.turn.is_active() {
            return Err(HistoryMutationError::Busy);
        }

        let mut session = self.session.lock().await;
        let mut client_ids = self.client_user_message_ids.lock().await;
        let history_changed = !session.history().is_empty() || !client_ids.is_empty();
        session.clear();
        client_ids.clear();
        let model = session.model().to_string();
        drop(client_ids);
        drop(session);
        let outcomes_changed = {
            let mut outcomes = self
                .assistant_outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = !outcomes.is_empty();
            outcomes.clear();
            changed
        };
        let changed = history_changed || outcomes_changed;
        let context_window = self.context_windows.lock().await.get(&model).copied();

        self.persist_current().await;
        let _ = self.events.emit(SessionEvent::ConversationCleared);
        self.publish_context_usage(self.context_usage(0, context_window));
        Ok(changed)
    }

    pub async fn cumulative_usage(&self) -> SessionUsage {
        let usage = self.session.lock().await.total_usage().clone();
        SessionUsage {
            input: usage.input,
            output: usage.output,
            reasoning_output: usage.reasoning_output,
            thinking_bytes: usage.thinking_bytes,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            cost_usd_micros: usd_to_micros(usage.cost.total_usd),
        }
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
        let current_thinking = session.thinking().clone();
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            mutation_active: AtomicBool::new(false),
            session: tokio::sync::Mutex::new(session),
            turn: TurnController::default(),
            approvals,
            current_model: StdMutex::new(current_model),
            current_thinking: StdMutex::new(current_thinking),
            cwd,
            client_user_message_ids: tokio::sync::Mutex::new(Vec::new()),
            assistant_outcomes: StdMutex::new(Vec::new()),
            compaction,
            context_windows: tokio::sync::Mutex::new(context_windows),
            events,
            tool_lifecycle,
            runtime_options: StdMutex::new(Vec::new()),
            current_context_usage: StdMutex::new(None),
            persistence_generation: AtomicU64::new(0),
            persistence_gate: tokio::sync::Mutex::new(()),
            persistence_requested: AtomicU64::new(0),
            persistence_completed: AtomicU64::new(0),
            persistence,
        }
    }

    pub fn set_runtime_options(&self, options: Vec<RuntimeOption>) {
        *self
            .runtime_options
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = options;
    }

    pub fn runtime_options(&self) -> Vec<RuntimeOption> {
        self.runtime_options
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn current_model(&self) -> String {
        self.current_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn apply_runtime_option(
        &self,
        config_id: &str,
        value: RuntimeValue,
    ) -> Result<Vec<RuntimeOption>, RuntimeConfigError> {
        let option = self
            .runtime_options()
            .into_iter()
            .find(|option| option.id == config_id)
            .ok_or(RuntimeConfigError::UnknownOption)?;
        if !option.accepts(&value) {
            return Err(RuntimeConfigError::InvalidValue);
        }
        if self.turn.is_active() && !option.mutable_while_running {
            return Err(RuntimeConfigError::Busy);
        }

        let _lifecycle = self.lifecycle.lock().await;
        if self.turn.is_active() && !option.mutable_while_running {
            return Err(RuntimeConfigError::Busy);
        }
        match (config_id, &value) {
            ("model", RuntimeValue::String(model)) => {
                let mut session = self.session.lock().await;
                let context_window = self
                    .prepare_model(&mut session, model)
                    .await
                    .map_err(RuntimeConfigError::ApplyFailed)?;
                let history = session.history().to_vec();
                *self
                    .current_model
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = model.clone();
                drop(session);
                self.persist_current().await;
                // Changing models does not change the conversation bytes. Keep
                // the last provider-observed occupancy as the best available
                // count, but recompute reservation/utilization against the new
                // model window. The next provider response replaces this with
                // that model's exact token count.
                let used_tokens = self
                    .current_context_usage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .map(|usage| usage.prompt_tokens)
                    .unwrap_or_else(|| crate::compaction::estimate_tokens(&history));
                self.publish_context_usage(
                    self.context_usage(used_tokens, context_window)
                        .mark_estimated(),
                );
            }
            ("thinking", RuntimeValue::String(level)) => {
                let thinking =
                    ThinkingLevel::from_input(level).map_err(RuntimeConfigError::ApplyFailed)?;
                let mut session = self.session.lock().await;
                session.set_thinking(thinking.clone());
                *self
                    .current_thinking
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = thinking;
                drop(session);
                self.persist_current().await;
            }
            _ => return Err(RuntimeConfigError::UnsupportedOption),
        }

        let options = {
            let mut options = self
                .runtime_options
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(current) = options.iter_mut().find(|current| current.id == config_id) {
                current.value = value;
            } else {
                // A concurrent catalog refresh may replace this option after
                // validation. The provider mutation has already succeeded, so
                // restore the validated option instead of reporting failure.
                let mut applied = option;
                applied.value = value;
                options.push(applied);
            }
            options.clone()
        };
        let _ = self.events.emit(SessionEvent::RuntimeOptionsChanged {
            options: options.clone(),
        });
        Ok(options)
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

    pub fn context_usage(&self, used_tokens: u64, context_window: Option<u64>) -> ContextUsage {
        let model = self.current_model();
        let policy = self
            .compaction
            .policy_for(&model, context_window)
            .ok()
            .flatten();
        let (reservation, high_water) = policy
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
        ContextUsage::new(used_tokens, context_window, reservation, high_water)
    }

    pub(crate) fn publish_context_usage(&self, usage: ContextUsage) {
        *self
            .current_context_usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(usage.clone());
        let _ = self
            .events
            .emit(SessionEvent::ContextUsageChanged { usage });
    }

    /// Phase one samples the router, then captures a payload under the
    /// canonical lock order: AgentSession -> client ids -> outcome/thinking.
    /// No awaits occur after taking a standard mutex, and event handlers must
    /// never acquire these state locks. Sampling the monotonic sequence first
    /// means the watermark may understate newer captured state but can never
    /// claim an event that the payload snapshot predates. State-only mutations
    /// may share a watermark; the generation minted while holding these locks
    /// still gives their payload snapshots a total write order. The atomic
    /// permits test-only synthetic captures; production correctness requires
    /// minting while this complete lock set is held.
    async fn capture_persistence(&self) -> PersistenceCapture {
        let through_seq = self.events.latest_sequence();
        let session = self.session.lock().await;
        let client_user_message_ids = self.client_user_message_ids.lock().await;
        let assistant_outcomes = self
            .assistant_outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let thinking = self
            .current_thinking
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = self.persistence_generation.fetch_add(1, Ordering::AcqRel) + 1;
        PersistenceCapture {
            generation,
            through_seq,
            model: session.model().to_string(),
            thinking: thinking.as_str().to_string(),
            messages: session.history().to_vec(),
            cwd: self.cwd.clone(),
            client_user_message_ids: client_user_message_ids.clone(),
            assistant_outcomes: assistant_outcomes.clone(),
        }
    }

    /// Register a save only after the caller's canonical mutation is complete.
    /// Requests registered before the gate owner's capture are coalesced into
    /// that snapshot; later requests retain their own uncompleted ticket.
    pub(crate) async fn persist_current(&self) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let request = self.persistence_requested.fetch_add(1, Ordering::AcqRel) + 1;
        let _gate = self.persistence_gate.lock().await;
        if self.persistence_completed.load(Ordering::Acquire) >= request {
            return;
        }
        persistence.mark_dirty();

        let policy = persistence.retry_policy;
        let handled_request = self.persistence_requested.load(Ordering::Acquire);
        let mut attempts = 0usize;
        let mut delay = policy.initial_backoff;
        loop {
            let capture = self.capture_persistence().await;
            attempts += 1;
            // Every capture guard is dropped before phase two acquires
            // SessionPersistence.state and performs blocking I/O.
            // Cancellation can detach this waiter while spawn_blocking
            // finishes; the request remains incomplete/dirty so a later call
            // or task-1409 final save recaptures instead of trusting it.
            match Self::save_capture_blocking(persistence.clone(), capture).await {
                Ok(PersistenceSaveOutcome::Saved) => {
                    self.persistence_completed
                        .fetch_max(handled_request, Ordering::AcqRel);
                    persistence
                        .mark_clean_if_fully_handled(handled_request, &self.persistence_requested);
                    return;
                }
                Ok(PersistenceSaveOutcome::SkippedStale { stored_generation }) => {
                    // A newer generation was already written. Production gate
                    // ownership makes this unreachable, but synthetic test
                    // captures can race it; recapture instead of claiming clean.
                    self.persistence_generation
                        .fetch_max(stored_generation, Ordering::AcqRel);
                    persistence.mark_dirty();
                    if attempts >= policy.attempts {
                        tracing::warn!(
                            target: "daimonos::session_core",
                            event = "session_payload_stale_capture_exhausted",
                            attempts,
                            "session payload remains dirty after repeated stale captures"
                        );
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(policy.max_backoff);
                    continue;
                }
                Ok(PersistenceSaveOutcome::SkippedDeleted) => {
                    self.persistence_completed
                        .fetch_max(handled_request, Ordering::AcqRel);
                    return;
                }
                Ok(PersistenceSaveOutcome::Superseded) => {
                    self.persistence_completed
                        .fetch_max(handled_request, Ordering::AcqRel);
                    tracing::warn!(
                        target: "daimonos::session_core",
                        event = "session_writer_superseded",
                        "session persistence stopped because a newer runtime claimed ownership"
                    );
                    return;
                }
                Err(error) => {
                    let retryable = is_retryable_persistence_error(&error);
                    persistence.mark_degraded(retryable);
                    if !retryable || attempts >= policy.attempts {
                        self.persistence_completed
                            .fetch_max(handled_request, Ordering::AcqRel);
                        tracing::warn!(
                            target: "daimonos::session_core",
                            event = "session_payload_save_exhausted",
                            attempts,
                            retryable,
                            error = %error,
                            "session payload remains degraded after bounded save attempts"
                        );
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(policy.max_backoff);
                }
            }
        }
    }

    async fn save_capture_blocking(
        persistence: SessionPersistence,
        capture: PersistenceCapture,
    ) -> std::io::Result<PersistenceSaveOutcome> {
        tokio::task::spawn_blocking(move || persistence.save(&capture))
            .await
            .map_err(std::io::Error::other)?
    }

    #[cfg(test)]
    fn persist_capture(&self, capture: &PersistenceCapture) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        if let Err(error) = persistence.save(capture) {
            // Task 1406 replaces this dropped capture with bounded recapture
            // and retry for retryable failures.
            tracing::warn!(
                target: "daimonos::session_core",
                event = "session_payload_save_failed",
                error = %error,
                "session payload could not be persisted"
            );
        }
    }

    #[allow(dead_code)] // Consumed by task 1409's daemon lifecycle policy.
    /// Lifecycle callers may trust Clean only after canonical mutation code has
    /// registered its save through persist_current; final saves must use that
    /// same API rather than bypassing request registration.
    pub(crate) fn persistence_health(&self) -> PersistenceHealth {
        if self.persistence_requested.load(Ordering::Acquire)
            > self.persistence_completed.load(Ordering::Acquire)
        {
            return PersistenceHealth::Dirty;
        }
        self.persistence
            .as_ref()
            .map_or(PersistenceHealth::Clean, SessionPersistence::health)
    }

    pub(crate) fn initialize_persistence_generation(&self, generation: u64) {
        self.persistence_generation
            .store(generation, Ordering::Release);
    }

    #[cfg(test)]
    /// Synthetic fixture hook; intentionally bypasses canonical capture locks.
    pub fn persist(&self, model: &str, messages: &[Message], client_user_message_ids: &[String]) {
        let thinking = self
            .current_thinking
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcomes = self
            .assistant_outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.persist_capture(&PersistenceCapture {
            generation: self.persistence_generation.fetch_add(1, Ordering::AcqRel) + 1,
            through_seq: self.events.latest_sequence(),
            model: model.to_string(),
            thinking: thinking.as_str().to_string(),
            messages: messages.to_vec(),
            cwd: self.cwd.clone(),
            client_user_message_ids: client_user_message_ids.to_vec(),
            assistant_outcomes: outcomes.clone(),
        });
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
        use crate::session_protocol::{TimelineEntryKind, ToolCallStateStatus};
        use crate::session_timeline::TimelineReducer;

        let session = self.session.lock().await;
        let outcomes = self
            .assistant_outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut timeline = TimelineReducer::new(max_entries);
        let mut outcome_index = 0;
        let mut turn_started = false;
        for message in session.history() {
            let starts_turn = message.role == Role::User
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(_)));
            if starts_turn {
                if turn_started {
                    if let Some(outcome) = outcomes.get(outcome_index) {
                        timeline.push_reconstructed(TimelineEntryKind::Outcome {
                            outcome: outcome.clone(),
                        });
                    }
                    outcome_index += 1;
                }
                turn_started = true;
            }

            let mut pending_text: Option<(ReconstructedTextKind, String)> = None;
            let flush_text = |pending: &mut Option<(ReconstructedTextKind, String)>,
                              timeline: &mut TimelineReducer| {
                if let Some((kind, text)) = pending.take() {
                    timeline.push_reconstructed(kind.into_entry(text));
                }
            };
            for block in &message.content {
                match block {
                    ContentBlock::Text(text) => {
                        let kind = match message.role {
                            Role::User => ReconstructedTextKind::User,
                            Role::Assistant => ReconstructedTextKind::Assistant,
                        };
                        match &mut pending_text {
                            Some((pending_kind, pending)) if *pending_kind == kind => {
                                pending.push_str(text);
                            }
                            _ => {
                                flush_text(&mut pending_text, &mut timeline);
                                pending_text = Some((kind, text.clone()));
                            }
                        }
                    }
                    ContentBlock::Thinking(text) => {
                        let kind = ReconstructedTextKind::Thought;
                        match &mut pending_text {
                            Some((pending_kind, pending)) if *pending_kind == kind => {
                                pending.push_str(text);
                            }
                            _ => {
                                flush_text(&mut pending_text, &mut timeline);
                                pending_text = Some((kind, text.clone()));
                            }
                        }
                    }
                    ContentBlock::ToolCall { id, name, .. } => {
                        flush_text(&mut pending_text, &mut timeline);
                        timeline.start_reconstructed_tool(id.clone(), name.clone(), name.clone());
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        flush_text(&mut pending_text, &mut timeline);
                        let status = if content == crate::agent::INTERRUPTED_TOOL_RESULT {
                            ToolCallStateStatus::Cancelled
                        } else if *is_error {
                            ToolCallStateStatus::Failed
                        } else {
                            ToolCallStateStatus::Completed
                        };
                        timeline.update_reconstructed_tool(
                            tool_use_id,
                            status,
                            self.tool_lifecycle.project_output(content),
                        );
                    }
                    ContentBlock::Image { .. } | ContentBlock::ProviderState { .. } => {
                        flush_text(&mut pending_text, &mut timeline);
                    }
                }
            }
            flush_text(&mut pending_text, &mut timeline);
        }
        drop(session);
        if turn_started {
            if let Some(outcome) = outcomes.get(outcome_index) {
                timeline.push_reconstructed(TimelineEntryKind::Outcome {
                    outcome: outcome.clone(),
                });
            }
        }
        timeline.cancel_reconstructed_active_tools();
        let (timeline, active_tools, history_window) = timeline.into_parts();
        crate::session_protocol::SessionSnapshot {
            session_id,
            seq: self.events.latest_sequence(),
            turn_status: if self.turn.is_active() {
                TurnStatus::Running
            } else {
                TurnStatus::Idle
            },
            timeline,
            active_tools,
            history_window,
            pending_approvals: self.approvals.pending(),
            runtime_options: self.runtime_options(),
            context_usage: self
                .current_context_usage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
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

        let should_persist = turn.is_some();
        let cumulative_cost_usd = agent_session.total_usage().cost.total_usd;
        if turn.is_some() {
            let mut client_ids = self.client_user_message_ids.lock().await;
            client_ids.push(client_user_message_id.unwrap_or_default());
            align_client_user_message_ids(&mut client_ids, agent_session.user_turn_count());
        }
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
            self.publish_context_usage(self.context_usage(used_tokens, context_window));
            let outcome = outcome_mapper(turn);
            self.assistant_outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(outcome.clone());
            let _ = self.events.emit(SessionEvent::AssistantDone { outcome });
        }
        if should_persist {
            self.persist_current().await;
        }
        // Keep the active-turn permit through the terminal persistence write.
        // History mutations use this permit as their exclusion boundary, so
        // dropping it earlier could let /clear persist empty state and then be
        // overwritten by this turn's late snapshot.
        drop(active_turn);

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

fn usd_to_micros(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        if value != 0.0 {
            tracing::debug!(
                target: "daimonos::session_core",
                event = "invalid_cumulative_cost",
                value,
            );
        }
        return 0;
    }
    (value * 1_000_000.0).round().min(u64::MAX as f64) as u64
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReconstructedTextKind {
    User,
    Assistant,
    Thought,
}

impl ReconstructedTextKind {
    fn into_entry(self, text: String) -> crate::session_protocol::TimelineEntryKind {
        use crate::session_protocol::TimelineEntryKind;
        match self {
            Self::User => TimelineEntryKind::User {
                text,
                content_truncated: false,
            },
            Self::Assistant => TimelineEntryKind::Assistant {
                text,
                content_truncated: false,
            },
            Self::Thought => TimelineEntryKind::Thought {
                text,
                content_truncated: false,
            },
        }
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

    fn update_deadline(&self, approval_id: &str, deadline_unix_ms: u64, paused: bool) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = state.pending.get_mut(approval_id) else {
            return false;
        };
        debug_assert!(
            pending.request.ineligible_deadline_unix_ms.is_none()
                || pending.request.ineligible_deadline_unix_ms == Some(deadline_unix_ms),
            "approval deadline must remain anchored at first ineligibility"
        );
        if pending.request.ineligible_deadline_unix_ms == Some(deadline_unix_ms)
            && pending.request.deadline_paused == paused
        {
            return false;
        }
        pending.request.ineligible_deadline_unix_ms = Some(deadline_unix_ms);
        pending.request.deadline_paused = paused;
        true
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
fn system_deadline_unix_ms(timeout: std::time::Duration) -> u64 {
    let deadline = std::time::SystemTime::now()
        .checked_add(timeout)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    deadline
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

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
        let mut ineligible_deadline: Option<(tokio::time::Instant, u64)> = None;
        loop {
            // Tokio oneshot::Receiver::try_recv consumes only a ready value;
            // Empty leaves it valid for the cancel-safe &mut Receiver branch
            // below. Biased selects prefer resolution over simultaneous
            // eligibility churn.
            match receiver.try_recv() {
                Ok(resolution) => break resolution,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    return Err(broker_closed());
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
            let mut eligibility_changed = Box::pin(broker.eligibility_changed.notified());
            // notify_waiters does not retain a permit for a future that has
            // not registered yet. Enable before reading eligibility so every
            // transition after this point wakes this exact loop iteration.
            eligibility_changed.as_mut().enable();
            if broker.has_eligible_client(&approval_id) {
                if let Some((_, deadline_unix_ms)) = ineligible_deadline {
                    if broker.update_deadline(&approval_id, deadline_unix_ms, true) {
                        let _ = events.emit(SessionEvent::ApprovalDeadlineChanged {
                            approval_id: approval_id.clone(),
                            ineligible_deadline_unix_ms: deadline_unix_ms,
                            paused: true,
                        });
                    }
                }
                tokio::select! {
                    biased;
                    resolution = &mut receiver => {
                        break resolution.map_err(|_| broker_closed())?;
                    }
                    _ = &mut eligibility_changed => continue,
                }
            } else {
                let (deadline, deadline_unix_ms) = *ineligible_deadline.get_or_insert_with(|| {
                    (
                        tokio::time::Instant::now() + timeout,
                        system_deadline_unix_ms(timeout),
                    )
                });
                if broker.update_deadline(&approval_id, deadline_unix_ms, false) {
                    let _ = events.emit(SessionEvent::ApprovalDeadlineChanged {
                        approval_id: approval_id.clone(),
                        ineligible_deadline_unix_ms: deadline_unix_ms,
                        paused: false,
                    });
                }
                tokio::select! {
                    biased;
                    resolution = &mut receiver => {
                        break resolution.map_err(|_| broker_closed())?;
                    }
                    _ = &mut eligibility_changed => continue,
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
            ineligible_deadline_unix_ms: None,
            deadline_paused: false,
        }
    }

    #[test]
    fn cumulative_cost_uses_bounded_microdollar_wire_units() {
        assert_eq!(usd_to_micros(0.123_456_7), 123_457);
        assert_eq!(usd_to_micros(f64::NAN), 0);
        assert_eq!(usd_to_micros(-1.0), 0);
        assert_eq!(usd_to_micros(f64::INFINITY), 0);
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
    async fn approval_deadline_state_is_anchored_and_paused_not_reset() {
        let broker = std::sync::Arc::new(ApprovalBroker::new_with_timeout(
            false,
            std::time::Duration::from_secs(1),
        ));
        let events = std::sync::Arc::new(SessionEventRouter::default());
        let request_broker = std::sync::Arc::clone(&broker);
        let request_events = std::sync::Arc::clone(&events);
        let pending = tokio::spawn(async move {
            request_approval(&request_broker, &request_events, request()).await
        });

        let anchored = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(request) = broker.pending().pop() {
                    if let Some(deadline) = request.ineligible_deadline_unix_ms {
                        break (request.id, deadline);
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first ineligible deadline is advertised");

        broker.set_eligible_client_counts(1, 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let request = broker.pending().pop().unwrap();
                if request.deadline_paused {
                    assert_eq!(request.ineligible_deadline_unix_ms, Some(anchored.1));
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("eligible reconnect pauses the anchored deadline");

        broker.set_eligible_client_counts(0, 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let request = broker.pending().pop().unwrap();
                if !request.deadline_paused {
                    assert_eq!(request.ineligible_deadline_unix_ms, Some(anchored.1));
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("eligibility loss resumes without extending deadline");

        broker
            .resolve(
                &anchored.0,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            )
            .unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap().decision,
            ApprovalDecision::Deny
        );
    }

    #[tokio::test]
    async fn eligibility_restored_before_expiry_pauses_even_past_deadline() {
        let broker = std::sync::Arc::new(ApprovalBroker::new_with_timeout(
            false,
            std::time::Duration::from_millis(200),
        ));
        let events = std::sync::Arc::new(SessionEventRouter::default());
        let request_broker = std::sync::Arc::clone(&broker);
        let request_events = std::sync::Arc::clone(&events);
        let pending = tokio::spawn(async move {
            request_approval(&request_broker, &request_events, request()).await
        });
        let approval_id = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(request) = broker.pending().pop() {
                    if request.ineligible_deadline_unix_ms.is_some() {
                        break request.id;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        broker.set_eligible_client_counts(1, 0);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            broker
                .pending()
                .iter()
                .any(|request| request.id == approval_id),
            "eligible client pauses expiry even after anchored wall time passes"
        );
        broker
            .resolve(
                &approval_id,
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
                    retryable: false,
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
        let shutdown_handles = source
            .split("async fn shutdown_session_handles_with")
            .nth(1)
            .unwrap()
            .split("async fn shutdown_all_bridges")
            .next()
            .unwrap();
        assert!(shutdown_handles.contains("SESSION_END_REASON_ENGINE_SHUTDOWN"));
        let shutdown = source
            .split("async fn shutdown_all_bridges")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(shutdown.contains("shutdown_session_handles_with"));
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
    fn restored_duplicate_tool_ids_update_newest_nonterminal_occurrence() {
        let mut timeline = crate::session_timeline::TimelineReducer::new(8);
        timeline.start_reconstructed_tool(
            "duplicate".to_string(),
            "first".to_string(),
            "first".to_string(),
        );
        timeline.start_reconstructed_tool(
            "duplicate".to_string(),
            "second".to_string(),
            "second".to_string(),
        );
        timeline.update_reconstructed_tool(
            "duplicate",
            crate::session_protocol::ToolCallStateStatus::Completed,
            "first output".to_string(),
        );
        assert!(matches!(
            &timeline.timeline()[1].entry,
            crate::session_protocol::TimelineEntryKind::Tool {
                output: Some(output),
                ..
            } if output == "first output"
        ));
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

    fn persistence_capture(
        through_seq: u64,
        cwd: &std::path::Path,
        text: &str,
    ) -> PersistenceCapture {
        PersistenceCapture {
            generation: through_seq,
            through_seq,
            model: "model".to_string(),
            thinking: "medium".to_string(),
            messages: vec![Message::user(text)],
            cwd: cwd.to_path_buf(),
            client_user_message_ids: Vec::new(),
            assistant_outcomes: Vec::new(),
        }
    }

    struct PersistenceTestProvider;

    #[async_trait::async_trait]
    impl crate::providers::LlmProvider for PersistenceTestProvider {
        async fn complete(
            &self,
            _context: &crate::providers::Context,
            _options: &crate::providers::CompleteOpts,
        ) -> crate::providers::LlmResponse {
            crate::providers::LlmResponse {
                retryable: false,
                content: Vec::new(),
                stop_reason: crate::providers::StopReason::EndTurn,
                error_message: None,
                context_overflow: false,
                usage: crate::providers::Usage::default(),
            }
        }
    }

    fn persistence_test_core(
        cwd: &std::path::Path,
        persistence: SessionPersistence,
    ) -> Arc<SessionCore> {
        let config = Arc::new(crate::config::Config::default());
        let tool_session = crate::session::Session::new(cwd.to_path_buf(), config);
        let events = Arc::new(SessionEventRouter::default());
        let approvals = Arc::new(ApprovalBroker::new(false));
        let lifecycle = Arc::new(CanonicalToolLifecycle::new(
            Arc::clone(&events),
            Arc::clone(&approvals),
            Arc::new(crate::safety::SafetyPolicy::default()),
            4,
        ));
        Arc::new(SessionCore::new(
            crate::agent::AgentSession::new(
                Box::new(PersistenceTestProvider),
                tool_session,
                crate::agent::AgentConfig::default(),
            ),
            "model".to_string(),
            cwd.to_path_buf(),
            SessionCompaction::new(None, false),
            HashMap::new(),
            approvals,
            Some(persistence),
            events,
            lifecycle,
        ))
    }

    #[tokio::test]
    async fn reconstructed_timeline_matches_live_event_fold() {
        use crate::providers::{ContentBlock, Message, Role};
        use crate::session_protocol::{AssistantOutcome, SessionEvent, ToolCallStateStatus};

        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let persistence = SessionPersistence::new(
            "session-1",
            store,
            PersistenceRetryPolicy::new(
                3,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(2),
            ),
        );
        let core = persistence_test_core(directory.path(), persistence);
        core.session.lock().await.set_history(vec![
            Message::user("question"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text("before".to_string()),
                    ContentBlock::ToolCall {
                        id: "call_0".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_0".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                }],
            },
            Message::assistant("after"),
        ]);
        core.assistant_outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(AssistantOutcome::Completed);

        let restored = core.initial_snapshot("session-1".to_string(), 32).await;
        let mut live = crate::session_timeline::TimelineReducer::new(32);
        for event in [
            SessionEvent::UserMessage {
                text: "question".to_string(),
                request_id: None,
            },
            SessionEvent::AssistantDelta {
                text: "before".to_string(),
            },
            SessionEvent::ToolCallStarted {
                id: "call_0".to_string(),
                name: "read".to_string(),
                title: "read".to_string(),
                input_summary: None,
            },
            SessionEvent::ToolCallUpdated {
                id: "call_0".to_string(),
                status: ToolCallStateStatus::InProgress,
            },
            SessionEvent::ToolCallFinished {
                id: "call_0".to_string(),
                status: ToolCallStateStatus::Completed,
                output: "ok".to_string(),
            },
            SessionEvent::AssistantDelta {
                text: "after".to_string(),
            },
            SessionEvent::AssistantDone {
                outcome: AssistantOutcome::Completed,
            },
        ] {
            live.apply(event);
        }

        assert_eq!(restored.timeline, live.timeline());
        assert_eq!(restored.active_tools, live.active_tools());
        assert_eq!(restored.history_window, *live.history_window());
    }

    #[tokio::test]
    async fn retryable_save_failure_recaptures_and_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::new(
                3,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(2),
            ),
        );
        persistence.fail_saves([std::io::ErrorKind::Interrupted]);
        let core = persistence_test_core(directory.path(), persistence);

        core.persist_current().await;

        assert_eq!(core.persistence_generation.load(Ordering::Acquire), 2);
        assert_eq!(core.persistence_health(), PersistenceHealth::Clean);
        assert!(store.load("session-1").is_some());
    }

    #[tokio::test]
    async fn nonretryable_save_failure_remains_degraded() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::new(
                3,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(2),
            ),
        );
        persistence.fail_saves([std::io::ErrorKind::PermissionDenied]);
        let core = persistence_test_core(directory.path(), persistence);

        core.persist_current().await;

        assert_eq!(core.persistence_generation.load(Ordering::Acquire), 1);
        assert_eq!(
            core.persistence_health(),
            PersistenceHealth::Degraded { retryable: false }
        );
        assert!(store.load("session-1").is_none());
    }

    #[tokio::test]
    async fn retryable_save_failure_exhausts_configured_attempt_bound() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::new(3, std::time::Duration::ZERO, std::time::Duration::ZERO),
        );
        persistence.fail_saves([
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Interrupted,
        ]);
        let core = persistence_test_core(directory.path(), persistence);

        core.persist_current().await;

        assert_eq!(core.persistence_generation.load(Ordering::Acquire), 3);
        assert_eq!(
            core.persistence_health(),
            PersistenceHealth::Degraded { retryable: true }
        );
        assert_eq!(core.persistence_completed.load(Ordering::Acquire), 1);
        assert!(store.load("session-1").is_none());
    }

    #[test]
    fn persistence_error_retryability_is_explicit() {
        for kind in [
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(is_retryable_persistence_error(&std::io::Error::from(kind)));
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::StorageFull,
        ] {
            assert!(!is_retryable_persistence_error(&std::io::Error::from(kind)));
        }
    }

    #[tokio::test]
    async fn concurrent_save_requests_coalesce_to_latest_capture() {
        let directory = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            "session-1",
            SessionStore::new(directory.path().join("sessions")),
            PersistenceRetryPolicy::single_attempt(),
        );
        let core = persistence_test_core(directory.path(), persistence);
        let gate = core.persistence_gate.lock().await;
        let first_core = Arc::clone(&core);
        let first = tokio::spawn(async move { first_core.persist_current().await });
        let second_core = Arc::clone(&core);
        let second = tokio::spawn(async move { second_core.persist_current().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while core.persistence_requested.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(gate);
        first.await.unwrap();
        second.await.unwrap();

        assert_eq!(core.persistence_generation.load(Ordering::Acquire), 1);
        assert_eq!(core.persistence_health(), PersistenceHealth::Clean);
    }

    #[tokio::test]
    async fn request_registered_after_capture_gets_its_own_save() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::single_attempt(),
        );
        let (save_started, release_save) = persistence.pause_next_save();
        let core = persistence_test_core(directory.path(), persistence);

        let first_core = Arc::clone(&core);
        let first = tokio::spawn(async move { first_core.persist_current().await });
        tokio::task::spawn_blocking(move || {
            save_started.recv_timeout(std::time::Duration::from_secs(1))
        })
        .await
        .unwrap()
        .unwrap();
        core.session
            .lock()
            .await
            .set_history(vec![Message::user("new request")]);
        let second_core = Arc::clone(&core);
        let second = tokio::spawn(async move { second_core.persist_current().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while core.persistence_requested.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release_save.send(()).unwrap();
        first.await.unwrap();
        second.await.unwrap();

        assert_eq!(core.persistence_generation.load(Ordering::Acquire), 2);
        assert_eq!(core.persistence_health(), PersistenceHealth::Clean);
        let loaded = store.load("session-1").unwrap();
        assert!(matches!(
            loaded.messages[0].content.as_slice(),
            [crate::providers::ContentBlock::Text(text)] if text == "new request"
        ));
    }

    #[tokio::test]
    async fn stale_generation_fast_forwards_and_recaptures() {
        let directory = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            "session-1",
            SessionStore::new(directory.path().join("sessions")),
            PersistenceRetryPolicy::new(2, std::time::Duration::ZERO, std::time::Duration::ZERO),
        );
        persistence.state.lock().unwrap().last_saved_generation = Some(100);
        let core = persistence_test_core(directory.path(), persistence);

        core.persist_current().await;

        assert_eq!(core.persistence_generation.load(Ordering::Acquire), 101);
        assert_eq!(core.persistence_health(), PersistenceHealth::Clean);
        assert_eq!(core.persistence_completed.load(Ordering::Acquire), 1);
        assert_eq!(
            SessionStore::new(directory.path().join("sessions"))
                .load_result("session-1")
                .unwrap()
                .generation,
            101
        );
    }

    #[tokio::test]
    async fn reopened_writer_epoch_rejects_delayed_old_core_save() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let old_persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::single_attempt(),
        );
        let old_core = persistence_test_core(directory.path(), old_persistence.clone());
        old_core.persist_current().await;
        let (save_started, release_save) = old_persistence.pause_next_save();
        let delayed_core = Arc::clone(&old_core);
        let delayed = tokio::spawn(async move { delayed_core.persist_current().await });
        tokio::task::spawn_blocking(move || {
            save_started
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
        })
        .await
        .unwrap();

        let new_persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::single_attempt(),
        );
        let new_core = persistence_test_core(directory.path(), new_persistence);
        new_core.initialize_persistence_generation(1);
        new_core
            .session
            .lock()
            .await
            .set_history(vec![Message::user("new runtime")]);
        release_save.send(()).unwrap();
        delayed.await.unwrap();
        new_core.persist_current().await;

        assert_eq!(old_core.persistence_health(), PersistenceHealth::Superseded);
        let loaded = store.load_result("session-1").unwrap();
        assert_eq!(loaded.generation, 2);
        assert!(matches!(
            loaded.messages[0].content.as_slice(),
            [crate::providers::ContentBlock::Text(text)] if text == "new runtime"
        ));
    }

    #[tokio::test]
    async fn daemon_persistence_updates_and_tombstones_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let catalog = match crate::session_catalog::SessionCatalog::open(
            directory.path().join("catalog.sqlite"),
            std::time::Duration::from_secs(1),
        )
        .unwrap()
        {
            crate::session_catalog::CatalogOpen::Ready(catalog) => catalog,
            crate::session_catalog::CatalogOpen::NewerSchema { .. } => panic!("fresh catalog"),
        };
        let writer = crate::session_catalog::SessionCatalogWriter::start(
            catalog,
            "workspace".to_string(),
            8,
            4,
            64,
        );
        let persistence =
            SessionPersistence::new("session-1", store, PersistenceRetryPolicy::single_attempt())
                .with_catalog_writer(Arc::clone(&writer));
        let mut capture = persistence_capture(1, directory.path(), "hello");
        capture.client_user_message_ids = vec!["prompt-1".to_string()];
        assert_eq!(
            persistence.save(&capture).unwrap(),
            PersistenceSaveOutcome::Saved
        );
        assert!(
            writer
                .wait_until_quiet(std::time::Duration::from_secs(1))
                .await
        );
        let row = writer.catalog().row("session-1").unwrap().unwrap();
        assert!(!row.deleted);
        assert_eq!(row.preview.as_deref(), Some("hello"));

        assert!(persistence.delete().unwrap());
        assert!(
            writer
                .wait_until_quiet(std::time::Duration::from_secs(1))
                .await
        );
        assert!(writer.catalog().row("session-1").unwrap().unwrap().deleted);
    }

    #[test]
    fn persistence_writer_rejects_lower_generation_and_orders_equal_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::single_attempt(),
        );

        assert_eq!(
            persistence
                .save(&persistence_capture(2, directory.path(), "newer"))
                .unwrap(),
            PersistenceSaveOutcome::Saved
        );
        assert_eq!(
            persistence
                .save(&persistence_capture(1, directory.path(), "stale"))
                .unwrap(),
            PersistenceSaveOutcome::SkippedStale {
                stored_generation: 2
            }
        );
        let loaded = store.load("session-1").unwrap();
        assert!(matches!(
            loaded.messages[0].content.as_slice(),
            [crate::providers::ContentBlock::Text(text)] if text == "newer"
        ));

        let mut equal_sequence = persistence_capture(2, directory.path(), "equal");
        equal_sequence.generation = 3;
        assert_eq!(
            persistence.save(&equal_sequence).unwrap(),
            PersistenceSaveOutcome::Saved
        );
        let loaded = store.load("session-1").unwrap();
        assert!(matches!(
            loaded.messages[0].content.as_slice(),
            [crate::providers::ContentBlock::Text(text)] if text == "equal"
        ));
    }

    #[test]
    fn failed_write_does_not_advance_sequence_watermark() {
        let directory = tempfile::tempdir().unwrap();
        let store_path = directory.path().join("sessions");
        let displaced_store_path = directory.path().join("sessions-displaced");
        let store = SessionStore::new(store_path.clone());
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::single_attempt(),
        );
        std::fs::rename(&store_path, &displaced_store_path).unwrap();
        std::fs::write(&store_path, b"file").unwrap();

        assert!(persistence
            .save(&persistence_capture(2, directory.path(), "failed"))
            .is_err());
        std::fs::remove_file(&store_path).unwrap();
        std::fs::rename(&displaced_store_path, &store_path).unwrap();
        assert_eq!(
            persistence
                .save(&persistence_capture(1, directory.path(), "accepted"))
                .unwrap(),
            PersistenceSaveOutcome::Saved
        );
        let loaded = store.load("session-1").unwrap();
        assert!(matches!(
            loaded.messages[0].content.as_slice(),
            [crate::providers::ContentBlock::Text(text)] if text == "accepted"
        ));
    }

    #[tokio::test]
    async fn failed_payload_write_never_enters_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let store_path = directory.path().join("sessions");
        let displaced_store_path = directory.path().join("sessions-displaced");
        let store = SessionStore::new(store_path.clone());
        let catalog = match crate::session_catalog::SessionCatalog::open(
            directory.path().join("catalog.sqlite"),
            std::time::Duration::from_secs(1),
        )
        .unwrap()
        {
            crate::session_catalog::CatalogOpen::Ready(catalog) => catalog,
            crate::session_catalog::CatalogOpen::NewerSchema { .. } => panic!("fresh catalog"),
        };
        let writer = crate::session_catalog::SessionCatalogWriter::start(
            catalog,
            "workspace".to_string(),
            8,
            4,
            64,
        );
        let persistence =
            SessionPersistence::new("session-1", store, PersistenceRetryPolicy::single_attempt())
                .with_catalog_writer(Arc::clone(&writer));
        std::fs::rename(&store_path, &displaced_store_path).unwrap();
        std::fs::write(&store_path, b"file").unwrap();
        assert!(persistence
            .save(&persistence_capture(1, directory.path(), "hello"))
            .is_err());
        assert!(
            writer
                .wait_until_quiet(std::time::Duration::from_secs(1))
                .await
        );
        assert!(writer.catalog().row("session-1").unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_payload_delete_does_not_commit_state_or_tombstone() {
        let directory = tempfile::tempdir().unwrap();
        let store_path = directory.path().join("sessions");
        let displaced_store_path = directory.path().join("sessions-displaced");
        let catalog = match crate::session_catalog::SessionCatalog::open(
            directory.path().join("catalog.sqlite"),
            std::time::Duration::from_secs(1),
        )
        .unwrap()
        {
            crate::session_catalog::CatalogOpen::Ready(catalog) => catalog,
            crate::session_catalog::CatalogOpen::NewerSchema { .. } => panic!("fresh catalog"),
        };
        let writer = crate::session_catalog::SessionCatalogWriter::start(
            catalog,
            "workspace".to_string(),
            8,
            4,
            64,
        );
        let store = SessionStore::new(store_path.clone());
        let persistence = SessionPersistence::new(
            "session-1",
            store.clone(),
            PersistenceRetryPolicy::single_attempt(),
        )
        .with_catalog_writer(Arc::clone(&writer));
        std::fs::rename(&store_path, &displaced_store_path).unwrap();
        std::fs::write(&store_path, b"file").unwrap();

        assert!(persistence.delete().is_err());
        assert!(writer.catalog().row("session-1").unwrap().is_none());

        std::fs::remove_file(&store_path).unwrap();
        std::fs::rename(&displaced_store_path, &store_path).unwrap();
        assert!(!persistence.delete().unwrap());
        assert!(
            writer
                .wait_until_quiet(std::time::Duration::from_secs(1))
                .await
        );
        assert!(writer.catalog().row("session-1").unwrap().unwrap().deleted);
        assert_eq!(
            persistence
                .save(&persistence_capture(
                    2,
                    directory.path(),
                    "must not resurrect"
                ))
                .unwrap(),
            PersistenceSaveOutcome::SkippedDeleted
        );
        assert!(store.load("session-1").is_none());
        assert!(
            writer
                .wait_until_quiet(std::time::Duration::from_secs(1))
                .await
        );
        assert!(writer.catalog().row("session-1").unwrap().unwrap().deleted);
    }
}
