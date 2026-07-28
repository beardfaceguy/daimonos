use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::oneshot;

use crate::agent::AgentSession;
use crate::compaction::CompactionPolicy;
use crate::providers::Message;
use crate::session_protocol::{ApprovalDecision, ApprovalRequest, ClientCapability, SessionEvent};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventError {
    SequenceExhausted,
    HandlerPanicked,
}

pub type SessionEventHandler = std::sync::Arc<dyn Fn(u64, SessionEvent) + Send + Sync + 'static>;

/// Transport-independent projection of canonical session events.
///
/// The router assigns session-local monotonic sequence numbers before handing
/// events to the configured adapter. Reliable replay/ring retention is layered
/// on this sequence in Vikunja #1100; this layer owns only canonical ordering.
pub struct SessionEventRouter {
    sequence: StdMutex<u64>,
    handler: Option<SessionEventHandler>,
}

impl SessionEventRouter {
    pub fn new(handler: Option<SessionEventHandler>) -> Self {
        Self {
            sequence: StdMutex::new(0),
            handler,
        }
    }

    pub fn emit(&self, event: SessionEvent) -> Result<u64, SessionEventError> {
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
        if let Some(handler) = &self.handler {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handler(sequence, event);
            }))
            .map_err(|_| SessionEventError::HandlerPanicked)?;
        }
        Ok(sequence)
    }

    pub fn latest_sequence(&self) -> u64 {
        *self
            .sequence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
}

impl SessionPersistence {
    pub fn new(session_id: impl Into<String>, store: SessionStore) -> Self {
        Self {
            session_id: session_id.into(),
            store,
        }
    }

    fn save(
        &self,
        model: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
    ) {
        self.store.save_acp(
            &self.session_id,
            model,
            messages,
            cwd,
            client_user_message_ids,
        );
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
    pub(crate) compaction: SessionCompaction,
    pub(crate) context_windows: tokio::sync::Mutex<HashMap<String, u64>>,
    pub(crate) events: Arc<SessionEventRouter>,
    persistence: Option<SessionPersistence>,
}

impl SessionCore {
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
    ) -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            session: tokio::sync::Mutex::new(session),
            turn: TurnController::default(),
            approvals,
            current_model: StdMutex::new(current_model),
            cwd,
            client_user_message_ids: tokio::sync::Mutex::new(Vec::new()),
            compaction,
            context_windows: tokio::sync::Mutex::new(context_windows),
            events,
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
            persistence.save(model, messages, &self.cwd, client_user_message_ids);
        }
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
    next_id: u64,
}

pub struct ApprovalBroker {
    state: StdMutex<ApprovalState>,
    allow_always: bool,
}

impl ApprovalBroker {
    pub fn new(allow_always: bool) -> Self {
        Self {
            state: StdMutex::new(ApprovalState::default()),
            allow_always,
        }
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
            // Deliberately identical for unknown and already-resolved ids: both
            // are safe no-ops, and the response does not disclose broker history.
            return Err(ApprovalError::NotPending);
        };
        if decision == ApprovalDecision::AllowAlways && !pending.request.allow_always_available {
            return Err(ApprovalError::AllowAlwaysUnavailable);
        }
        let Some(pending) = state.pending.remove(approval_id) else {
            return Err(ApprovalError::NotPending);
        };
        let resolution = ApprovalResolution {
            approval_id: approval_id.to_string(),
            decision,
            resolved_by: resolved_by.to_string(),
        };
        let _ = pending.sender.send(resolution.clone());
        Ok(resolution)
    }

    pub fn cancel_all(&self, resolved_by: &str) -> Vec<ApprovalResolution> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending: Vec<_> = state.pending.drain().collect();
        pending.sort_by(|left, right| left.0.cmp(&right.0));
        let mut resolutions = Vec::with_capacity(pending.len());
        for (approval_id, pending) in pending {
            let resolution = ApprovalResolution {
                approval_id,
                decision: ApprovalDecision::Deny,
                resolved_by: resolved_by.to_string(),
            };
            let _ = pending.sender.send(resolution.clone());
            resolutions.push(resolution);
        }
        resolutions
    }
}

impl Drop for ApprovalBroker {
    fn drop(&mut self) {
        let _ = self.cancel_all("broker_drop");
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
        assert_eq!(broker.pending(), vec![registered.request.clone()]);
        assert!(!registered.request.allow_always_available);

        let resolution = broker
            .resolve(
                &registered.request.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            )
            .unwrap();
        assert_eq!(registered.receiver.await.unwrap(), resolution);
        assert!(broker.pending().is_empty());
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
    fn first_resolution_wins_and_late_or_unknown_answers_are_safe_noops() {
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
            Err(ApprovalError::NotPending)
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
            ("UserMessage", "emit(CoreSessionEvent::UserMessage"),
            ("AssistantDelta", "emit(CoreSessionEvent::AssistantDelta"),
            ("AssistantDone", "emit(CoreSessionEvent::AssistantDone"),
            ("ThoughtDelta", "emit(CoreSessionEvent::ThoughtDelta"),
            ("ToolCallStarted", "emit(CoreSessionEvent::ToolCallStarted"),
            ("ToolCallUpdated", "emit(CoreSessionEvent::ToolCallUpdated"),
            (
                "ToolCallFinished",
                "emit(CoreSessionEvent::ToolCallFinished",
            ),
            (
                "ApprovalRequested",
                "emit(CoreSessionEvent::ApprovalRequested",
            ),
            (
                "ApprovalResolved",
                "emit(CoreSessionEvent::ApprovalResolved",
            ),
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
        assert!(prompt.contains("emit_assistant_done"));
        assert!(retry.contains("emit_assistant_done"));
        assert!(prompt.contains("cleanup_cancelled_turn"));
        assert!(retry.contains("cleanup_cancelled_turn"));

        let permission = source
            .split("async fn request_permission")
            .nth(1)
            .unwrap()
            .split("fn build_before_tool_call_hook")
            .next()
            .unwrap();
        assert!(permission.contains("CoreTurnStatus::WaitingForApproval"));
        assert!(permission.contains("CoreTurnStatus::Running"));

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
