//! Thin TUI adapter over the daemon-session controller (Vikunja #1331).
//!
//! The adapter owns no provider, tools, approvals, or persistence. It accepts
//! only canonical daemon snapshots/events and exposes the resulting
//! [`ViewState`](crate::frontend_state::ViewState) to rendering.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::frontend_state::ViewState;
use crate::session_client::SessionClientOutcome;
use crate::session_controller::{
    ControllerSendError, EpochEvent, SessionControllerCommand, SessionControllerEvent,
    SessionControllerHandle,
};
use crate::session_protocol::{AttachDeniedCode, RuntimeValue, TurnStatus};

pub type ControllerFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<SessionControllerHandle>> + Send + 'static>>;
pub type ControllerFactory = Arc<dyn Fn() -> ControllerFuture + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy)]
pub struct SwitchPolicy {
    pub retry_attempts: usize,
    pub retry_backoff: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiSessionUpdate {
    Updated,
    Detached,
    Failed(String),
    Stopped,
}

pub struct TuiSession {
    active: ActiveSession,
    command_timeout: Duration,
    controller_factory: Option<ControllerFactory>,
    switch_policy: SwitchPolicy,
    last_detached_session: Option<String>,
}

struct ActiveSession {
    controller: SessionControllerHandle,
    epoch: u64,
    state: ViewState,
    termination_reported: bool,
}

enum CandidateAttachError {
    Denied {
        code: Option<AttachDeniedCode>,
        message: String,
    },
    Other(String),
}

impl TuiSession {
    pub async fn attach(
        controller: SessionControllerHandle,
        command_timeout: Duration,
    ) -> Result<Self, anyhow::Error> {
        Self::attach_with_switching(
            controller,
            command_timeout,
            None,
            SwitchPolicy {
                retry_attempts: 1,
                retry_backoff: Duration::ZERO,
            },
        )
        .await
    }

    pub async fn attach_with_switching(
        controller: SessionControllerHandle,
        command_timeout: Duration,
        controller_factory: Option<ControllerFactory>,
        switch_policy: SwitchPolicy,
    ) -> Result<Self, anyhow::Error> {
        Self::attach_to(
            controller,
            None,
            command_timeout,
            controller_factory,
            switch_policy,
        )
        .await
        .map_err(|error| match error {
            CandidateAttachError::Denied { code, message } => {
                anyhow::anyhow!("daemon attach denied ({code:?}): {message}")
            }
            CandidateAttachError::Other(message) => anyhow::anyhow!(message),
        })
    }

    async fn attach_to(
        mut controller: SessionControllerHandle,
        session_id: Option<String>,
        command_timeout: Duration,
        controller_factory: Option<ControllerFactory>,
        switch_policy: SwitchPolicy,
    ) -> Result<Self, CandidateAttachError> {
        let attach = async move {
            let epoch = controller.epoch();
            controller
                .send(SessionControllerCommand::Attach { session_id })
                .await
                .map_err(|error| {
                    CandidateAttachError::Other(format!("failed to queue daemon attach: {error:?}"))
                })?;
            loop {
                let event = controller.recv().await.ok_or_else(|| {
                    CandidateAttachError::Other(
                        "candidate connection closed during handshake; daemon connection \
                             capacity may be exhausted or the daemon may be unavailable"
                            .to_string(),
                    )
                })?;
                if event.epoch != epoch {
                    continue;
                }
                match event.event {
                    SessionControllerEvent::Attached { state, .. } => {
                        return Ok(Self {
                            active: ActiveSession {
                                controller,
                                epoch,
                                state,
                                termination_reported: false,
                            },
                            command_timeout,
                            controller_factory,
                            switch_policy,
                            last_detached_session: None,
                        });
                    }
                    SessionControllerEvent::AttachFailed { code, message } => {
                        return Err(CandidateAttachError::Denied { code, message });
                    }
                    SessionControllerEvent::Failed { message, .. } => {
                        return Err(CandidateAttachError::Other(format!(
                            "daemon attach failed: {message}"
                        )));
                    }
                    SessionControllerEvent::Stopped => {
                        return Err(CandidateAttachError::Other(
                            "session controller stopped during attach".to_string(),
                        ));
                    }
                    SessionControllerEvent::StateChanged { .. }
                    | SessionControllerEvent::Reconnected { .. }
                    | SessionControllerEvent::CommandAccepted { .. }
                    | SessionControllerEvent::CommandRejected { .. }
                    | SessionControllerEvent::Detached => {}
                }
            }
        };
        tokio::time::timeout(command_timeout, attach)
            .await
            .map_err(|_| {
                CandidateAttachError::Other(
                    "timed out waiting for daemon attach; increase \
                     session.client_command_timeout_secs if the daemon is healthy but slow"
                        .to_string(),
                )
            })?
    }

    pub fn state(&self) -> &ViewState {
        &self.active.state
    }

    pub fn state_mut(&mut self) -> &mut ViewState {
        &mut self.active.state
    }

    pub async fn switch_to(&mut self, target_session_id: &str) -> anyhow::Result<()> {
        if target_session_id == self.active.state.session_id() {
            self.send(SessionControllerCommand::Sync)
                .await
                .map_err(|error| anyhow::anyhow!("failed to request session resync: {error:?}"))?;
            return Ok(());
        }
        self.ensure_switch_allowed()?;
        let factory = self
            .controller_factory
            .clone()
            .ok_or_else(|| anyhow::anyhow!("session switching is unavailable"))?;
        let transient_retry = self.last_detached_session.as_deref() == Some(target_session_id);
        let attempts = if transient_retry {
            self.switch_policy.retry_attempts.max(1)
        } else {
            1
        };

        for attempt in 0..attempts {
            if attempt > 0 {
                let multiplier = 1_u32 << attempt.saturating_sub(1).min(31);
                tokio::time::sleep(self.switch_policy.retry_backoff.saturating_mul(multiplier))
                    .await;
            }
            let controller = factory().await?;
            match Self::attach_to(
                controller,
                Some(target_session_id.to_string()),
                self.command_timeout,
                Some(Arc::clone(&factory)),
                self.switch_policy,
            )
            .await
            {
                Ok(candidate) => {
                    self.drain_before_switch_commit()?;
                    self.ensure_switch_allowed()?;
                    let departed_session = self.active.state.session_id().to_string();
                    let old_active = std::mem::replace(&mut self.active, candidate.active);
                    self.last_detached_session = Some(departed_session);
                    tokio::spawn(old_active.controller.shutdown());
                    return Ok(());
                }
                Err(CandidateAttachError::Denied {
                    code: Some(AttachDeniedCode::ClientLimitReached),
                    ..
                }) if transient_retry && attempt + 1 < attempts => continue,
                Err(CandidateAttachError::Denied {
                    code: Some(AttachDeniedCode::ClientLimitReached),
                    message,
                }) if transient_retry => {
                    anyhow::bail!(
                        "target session may still contain this TUI's departing attachment after \
                         {attempts} attempts: {message}"
                    );
                }
                Err(CandidateAttachError::Denied { code, message }) => {
                    anyhow::bail!("target session attach denied ({code:?}): {message}");
                }
                Err(CandidateAttachError::Other(message)) => {
                    anyhow::bail!("{message}");
                }
            }
        }
        unreachable!("switch attempts are always at least one")
    }

    fn ensure_switch_allowed(&self) -> anyhow::Result<()> {
        if !matches!(
            self.active.state.turn_status(),
            TurnStatus::Idle | TurnStatus::Cancelled
        ) {
            anyhow::bail!("cannot switch sessions while a turn is active");
        }
        if !self.active.state.pending_approvals().is_empty() {
            anyhow::bail!("cannot switch sessions while an approval is pending");
        }
        Ok(())
    }

    fn drain_before_switch_commit(&mut self) -> anyhow::Result<()> {
        while let Some(update) = self.poll() {
            match update {
                TuiSessionUpdate::Updated => {}
                TuiSessionUpdate::Failed(message) => anyhow::bail!("{message}"),
                TuiSessionUpdate::Detached | TuiSessionUpdate::Stopped => {
                    anyhow::bail!("current session disconnected during switch")
                }
            }
        }
        Ok(())
    }

    pub fn try_send(&self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
        self.active.controller.try_send(command)
    }

    pub async fn send(&self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
        self.active.controller.send(command).await
    }

    pub async fn set_config(
        &mut self,
        config_id: impl Into<String>,
        value: RuntimeValue,
    ) -> anyhow::Result<()> {
        self.request(
            SessionControllerCommand::SetConfig {
                config_id: config_id.into(),
                value,
            },
            "set_config",
        )
        .await
    }

    pub async fn stop(&mut self) -> anyhow::Result<()> {
        self.request(SessionControllerCommand::StopSession, "stop_session")
            .await
    }

    async fn request(
        &mut self,
        command: SessionControllerCommand,
        expected_operation: &'static str,
    ) -> anyhow::Result<()> {
        self.send(command)
            .await
            .map_err(|error| anyhow::anyhow!("failed to queue {expected_operation}: {error:?}"))?;
        let timeout = self.command_timeout;
        let wait = async {
            let mut accepted_request_id = None;
            loop {
                let event = self
                    .active
                    .controller
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!(self.controller_stop_message()))?;
                if event.epoch != self.active.epoch {
                    continue;
                }
                match event.event {
                    SessionControllerEvent::CommandAccepted {
                        operation,
                        request_id: Some(request_id),
                    } if operation == expected_operation => {
                        accepted_request_id = Some(request_id);
                    }
                    SessionControllerEvent::StateChanged { state, outcome } => {
                        self.active.state = state;
                        if let SessionClientOutcome::CommandResult {
                            request_id,
                            operation,
                            ..
                        } = outcome
                        {
                            if operation == expected_operation
                                && accepted_request_id.as_deref() == Some(request_id.as_str())
                            {
                                return Ok(());
                            }
                        }
                    }
                    SessionControllerEvent::Failed { operation, message }
                        if operation == expected_operation =>
                    {
                        anyhow::bail!("daemon rejected {expected_operation}: {message}")
                    }
                    SessionControllerEvent::CommandRejected { operation, message }
                        if operation == expected_operation =>
                    {
                        anyhow::bail!("{operation}: {message}")
                    }
                    SessionControllerEvent::Detached | SessionControllerEvent::Stopped => {
                        anyhow::bail!("{}", self.controller_stop_message());
                    }
                    SessionControllerEvent::Attached { .. }
                    | SessionControllerEvent::Reconnected { .. } => {
                        anyhow::bail!(
                            "session attachment changed while waiting for {expected_operation}"
                        );
                    }
                    SessionControllerEvent::CommandAccepted { .. }
                    | SessionControllerEvent::CommandRejected { .. }
                    | SessionControllerEvent::AttachFailed { .. }
                    | SessionControllerEvent::Failed { .. } => {}
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| {
            anyhow::anyhow!(
                "timed out waiting for daemon {expected_operation}; increase \
                     session.client_command_timeout_secs if the daemon is healthy but slow"
            )
        })?
    }

    pub fn poll(&mut self) -> Option<TuiSessionUpdate> {
        match self.active.controller.try_recv() {
            Ok(event) => Some(self.apply(event)),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                Some(self.disconnected_update())
            }
        }
    }

    pub async fn next_update(&mut self) -> TuiSessionUpdate {
        match self.active.controller.recv().await {
            Some(event) => self.apply(event),
            None => self.disconnected_update(),
        }
    }

    fn disconnected_update(&mut self) -> TuiSessionUpdate {
        if self.active.termination_reported {
            TuiSessionUpdate::Stopped
        } else {
            self.active.termination_reported = true;
            TuiSessionUpdate::Failed(self.controller_stop_message())
        }
    }

    fn controller_stop_message(&self) -> String {
        self.active
            .controller
            .stop_reason()
            .map(|reason| reason.to_string())
            .unwrap_or_else(|| "session controller stopped".to_string())
    }

    fn apply(&mut self, event: EpochEvent) -> TuiSessionUpdate {
        if event.epoch != self.active.epoch {
            return TuiSessionUpdate::Updated;
        }
        match event.event {
            SessionControllerEvent::Attached { state, .. } => {
                self.active.state = state;
                TuiSessionUpdate::Updated
            }
            SessionControllerEvent::Reconnected {
                mut state, message, ..
            } => {
                state.push_system_message(message);
                self.active.state = state;
                TuiSessionUpdate::Updated
            }
            SessionControllerEvent::StateChanged { mut state, outcome } => {
                match outcome {
                    SessionClientOutcome::Revoked { code, reason } => {
                        self.active.state = state;
                        return TuiSessionUpdate::Failed(format!(
                            "session revoked ({code:?}): {reason}"
                        ));
                    }
                    SessionClientOutcome::Usage { usage, .. } => {
                        state.push_system_message(format!(
                            "input={} output={} cache_read={} cache_write={} cost=${:.4}",
                            usage.input,
                            usage.output,
                            usage.cache_read,
                            usage.cache_write,
                            usage.cost_usd_micros as f64 / 1_000_000.0,
                        ));
                    }
                    _ => {}
                }
                self.active.state = state;
                TuiSessionUpdate::Updated
            }
            SessionControllerEvent::CommandAccepted { .. } => TuiSessionUpdate::Updated,
            SessionControllerEvent::CommandRejected { operation, message } => {
                self.active
                    .state
                    .push_system_message(format!("{operation}: {message}"));
                TuiSessionUpdate::Updated
            }
            SessionControllerEvent::AttachFailed { code, message } => {
                TuiSessionUpdate::Failed(format!("session attach denied ({code:?}): {message}"))
            }
            SessionControllerEvent::Detached => TuiSessionUpdate::Detached,
            SessionControllerEvent::Failed { operation, message } => {
                TuiSessionUpdate::Failed(format!("{operation}: {message}"))
            }
            SessionControllerEvent::Stopped => TuiSessionUpdate::Stopped,
        }
    }

    pub async fn shutdown(self) {
        self.active.controller.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::client_transport::{in_memory_transport_pair, ClientTransport};
    use crate::session_controller::SessionControllerHandle;
    use crate::session_protocol::{
        ClientCapability, ClientInfo, ClientKind, ClientMessage, ContextUsage, RevocationCode,
        ServerMessage, SessionEvent, SessionSnapshot, SessionUsage, TurnStatus, PROTOCOL_VERSION,
    };

    fn controller() -> (impl ClientTransport, SessionControllerHandle) {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let controller = SessionControllerHandle::spawn(
            transport,
            ClientInfo {
                id: "tui".to_string(),
                kind: ClientKind::Terminal,
                label: "terminal test".to_string(),
            },
            vec![ClientCapability::Observe, ClientCapability::Prompt],
            16,
            4,
            4,
        );
        (server, controller)
    }

    fn controller_factory(controllers: Vec<SessionControllerHandle>) -> ControllerFactory {
        let controllers = Arc::new(Mutex::new(VecDeque::from(controllers)));
        Arc::new(move || {
            let controllers = Arc::clone(&controllers);
            Box::pin(async move {
                controllers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("no candidate controller"))
            })
        })
    }

    async fn attach_switching(
        controller: SessionControllerHandle,
        server: &impl ClientTransport,
        session_id: &str,
        factory: ControllerFactory,
    ) -> TuiSession {
        let attach = tokio::spawn(async move {
            TuiSession::attach_with_switching(
                controller,
                Duration::from_secs(1),
                Some(factory),
                SwitchPolicy {
                    retry_attempts: 3,
                    retry_backoff: Duration::from_millis(5),
                },
            )
            .await
        });
        accept_attach(server, session_id).await;
        attach.await.unwrap().unwrap()
    }

    fn snapshot(session_id: &str) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.to_string(),
            seq: 0,
            turn_status: TurnStatus::Idle,
            transcript: Vec::new(),
            tool_calls: Vec::new(),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: Some(ContextUsage::new(0, Some(100), 0, None)),
            history_truncated: false,
        }
    }

    async fn accept_attach(server: &impl ClientTransport, session_id: &str) {
        let Some(ClientMessage::Attach { client, .. }) = server.recv().await.unwrap() else {
            panic!("expected attach");
        };
        assert_eq!(client.kind, ClientKind::Terminal);
        server
            .send(&ServerMessage::AttachOk {
                protocol_version: PROTOCOL_VERSION,
                session_id: session_id.to_string(),
                granted_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::Prompt,
                    ClientCapability::Configure,
                    ClientCapability::Stop,
                ],
                seq: 0,
            })
            .await
            .unwrap();
        server
            .send(&ServerMessage::Snapshot {
                seq: 0,
                state: snapshot(session_id),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn attach_uses_daemon_snapshot_as_the_only_initial_view() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let session = attach.await.unwrap().unwrap();
        assert_eq!(session.state().session_id(), "daemon-session");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn switch_commits_whole_candidate_then_detaches_old_controller() {
        let (old_server, old_controller) = controller();
        let (target_server, target_controller) = controller();
        let factory = controller_factory(vec![target_controller]);
        let session = attach_switching(old_controller, &old_server, "session-a", factory).await;
        let old_epoch = session.active.epoch;

        let switching = tokio::spawn(async move {
            let mut session = session;
            let result = session.switch_to("session-b").await;
            (result, session)
        });
        accept_attach(&target_server, "session-b").await;
        let (result, mut session) = switching.await.unwrap();
        result.unwrap();
        assert_eq!(session.state().session_id(), "session-b");
        let mut stale_state = ViewState::new("session-a");
        stale_state.apply_snapshot(snapshot("session-a"));
        session.apply(EpochEvent {
            epoch: old_epoch,
            event: SessionControllerEvent::Attached {
                state: stale_state,
                granted_capabilities: vec![ClientCapability::Observe],
            },
        });
        assert_eq!(
            session.state().session_id(),
            "session-b",
            "late old-epoch outcomes must not alter the committed candidate"
        );
        assert_eq!(
            old_server.recv().await.unwrap(),
            Some(ClientMessage::Detach)
        );
        session.shutdown().await;
    }

    #[tokio::test]
    async fn failed_candidate_attach_preserves_current_controller_and_view() {
        let (old_server, old_controller) = controller();
        let (target_server, target_controller) = controller();
        let factory = controller_factory(vec![target_controller]);
        let session = attach_switching(old_controller, &old_server, "session-a", factory).await;

        let switching = tokio::spawn(async move {
            let mut session = session;
            let result = session.switch_to("missing").await;
            (result, session)
        });
        let Some(ClientMessage::Attach { .. }) = target_server.recv().await.unwrap() else {
            panic!("expected candidate attach");
        };
        target_server
            .send(&ServerMessage::AttachDenied {
                code: Some(AttachDeniedCode::SessionNotFound),
                reason: "session was not found".to_string(),
            })
            .await
            .unwrap();

        let (result, session) = switching.await.unwrap();
        assert!(result.unwrap_err().to_string().contains("SessionNotFound"));
        assert_eq!(session.state().session_id(), "session-a");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_candidate_handshake_preserves_current_and_releases_transport() {
        let (old_server, old_controller) = controller();
        let (target_server, target_controller) = controller();
        let factory = controller_factory(vec![target_controller]);
        let mut session = attach_switching(old_controller, &old_server, "session-a", factory).await;

        let mut switching = Box::pin(session.switch_to("session-b"));
        let message = tokio::select! {
            message = target_server.recv() => message.unwrap(),
            result = &mut switching => panic!("switch finished before handshake cancellation: {result:?}"),
        };
        assert!(matches!(message, Some(ClientMessage::Attach { .. })));
        target_server
            .send(&ServerMessage::AttachOk {
                protocol_version: PROTOCOL_VERSION,
                session_id: "session-b".to_string(),
                granted_capabilities: vec![ClientCapability::Observe],
                seq: 0,
            })
            .await
            .unwrap();
        drop(switching);

        assert_eq!(session.state().session_id(), "session-a");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), target_server.recv())
                .await
                .expect("cancelled candidate transport should close")
                .unwrap(),
            None
        );
        session.shutdown().await;
    }

    #[tokio::test]
    async fn delivered_approval_before_commit_aborts_switch_without_mixing_state() {
        let (old_server, old_controller) = controller();
        let (target_server, target_controller) = controller();
        let factory = controller_factory(vec![target_controller]);
        let session = attach_switching(old_controller, &old_server, "session-a", factory).await;

        let switching = tokio::spawn(async move {
            let mut session = session;
            let result = session.switch_to("session-b").await;
            (result, session)
        });
        let Some(ClientMessage::Attach { .. }) = target_server.recv().await.unwrap() else {
            panic!("expected candidate attach");
        };
        old_server
            .send(&ServerMessage::Event {
                seq: 1,
                event: SessionEvent::ApprovalRequested {
                    request: crate::session_protocol::ApprovalRequest {
                        id: "approval-1".to_string(),
                        tool_call_id: "tool-1".to_string(),
                        tool: "exec".to_string(),
                        detail: "cargo test".to_string(),
                        allow_always_available: false,
                        ineligible_deadline_unix_ms: None,
                        deadline_paused: false,
                    },
                },
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        target_server
            .send(&ServerMessage::AttachOk {
                protocol_version: PROTOCOL_VERSION,
                session_id: "session-b".to_string(),
                granted_capabilities: vec![ClientCapability::Observe],
                seq: 0,
            })
            .await
            .unwrap();
        target_server
            .send(&ServerMessage::Snapshot {
                seq: 0,
                state: snapshot("session-b"),
            })
            .await
            .unwrap();

        let (result, session) = switching.await.unwrap();
        assert!(result.unwrap_err().to_string().contains("approval"));
        assert_eq!(session.state().session_id(), "session-a");
        assert_eq!(session.state().pending_approvals().len(), 1);
        session.shutdown().await;
    }

    #[tokio::test]
    async fn rapid_switch_back_retries_transient_client_limit() {
        let (server_a, controller_a) = controller();
        let (server_b, controller_b) = controller();
        let (server_a_limited, controller_a_limited) = controller();
        let (server_a_retry, controller_a_retry) = controller();
        let factory =
            controller_factory(vec![controller_b, controller_a_limited, controller_a_retry]);
        let session = attach_switching(controller_a, &server_a, "session-a", factory).await;

        let to_b = tokio::spawn(async move {
            let mut session = session;
            session.switch_to("session-b").await.unwrap();
            session
        });
        accept_attach(&server_b, "session-b").await;
        let session = to_b.await.unwrap();
        assert_eq!(session.state().session_id(), "session-b");

        let to_a = tokio::spawn(async move {
            let mut session = session;
            let result = session.switch_to("session-a").await;
            (result, session)
        });
        let Some(ClientMessage::Attach { .. }) = server_a_limited.recv().await.unwrap() else {
            panic!("expected first switch-back attach");
        };
        server_a_limited
            .send(&ServerMessage::AttachDenied {
                code: Some(AttachDeniedCode::ClientLimitReached),
                reason: "client limit reached".to_string(),
            })
            .await
            .unwrap();
        accept_attach(&server_a_retry, "session-a").await;

        let (result, session) = to_a.await.unwrap();
        result.unwrap();
        assert_eq!(session.state().session_id(), "session-a");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn unrelated_client_limit_is_not_retried() {
        let (old_server, old_controller) = controller();
        let (limited_server, limited_controller) = controller();
        let (unused_server, unused_controller) = controller();
        let factory = controller_factory(vec![limited_controller, unused_controller]);
        let session = attach_switching(old_controller, &old_server, "session-a", factory).await;

        let switching = tokio::spawn(async move {
            let mut session = session;
            let result = session.switch_to("session-c").await;
            (result, session)
        });
        let Some(ClientMessage::Attach { .. }) = limited_server.recv().await.unwrap() else {
            panic!("expected candidate attach");
        };
        limited_server
            .send(&ServerMessage::AttachDenied {
                code: Some(AttachDeniedCode::ClientLimitReached),
                reason: "client limit reached".to_string(),
            })
            .await
            .unwrap();
        let (result, session) = switching.await.unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("ClientLimitReached"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), unused_server.recv())
                .await
                .is_err(),
            "unrelated target must not consume a retry controller"
        );
        session.shutdown().await;
    }

    #[tokio::test]
    async fn selecting_current_session_requests_resync_without_new_controller() {
        let (server, controller) = controller();
        let session = attach_switching(
            controller,
            &server,
            "session-a",
            controller_factory(Vec::new()),
        )
        .await;
        let mut session = session;
        session.switch_to("session-a").await.unwrap();
        assert_eq!(
            server.recv().await.unwrap(),
            Some(ClientMessage::SyncRequest { last_seen_seq: 0 })
        );
        session.shutdown().await;
    }

    #[tokio::test]
    async fn detach_sends_wire_detach_without_stopping_the_session() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let session = attach.await.unwrap().unwrap();
        session.shutdown().await;
        assert_eq!(server.recv().await.unwrap(), Some(ClientMessage::Detach));
    }

    #[tokio::test]
    async fn prompts_and_canonical_events_flow_through_the_controller() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();

        session
            .try_send(SessionControllerCommand::Prompt {
                text: "hello".to_string(),
            })
            .unwrap();
        let Some(ClientMessage::Prompt { text, .. }) = server.recv().await.unwrap() else {
            panic!("expected prompt");
        };
        assert_eq!(text, "hello");
        server
            .send(&ServerMessage::Event {
                seq: 1,
                event: SessionEvent::UserMessage {
                    text: "hello".to_string(),
                    request_id: None,
                },
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if session.poll().is_some()
                    && session
                        .state()
                        .transcript()
                        .iter()
                        .any(|entry| entry.text == "hello")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canonical event should reach the TUI view");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn config_change_waits_for_daemon_result_before_returning() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();

        let change = tokio::spawn(async move {
            session
                .set_config("model", RuntimeValue::String("model-b".to_string()))
                .await
                .map(|()| session)
        });
        let Some(ClientMessage::SetConfig {
            request_id: Some(request_id),
            config_id,
            ..
        }) = server.recv().await.unwrap()
        else {
            panic!("expected set_config");
        };
        assert_eq!(config_id, "model");
        assert!(!change.is_finished());
        server
            .send(&ServerMessage::CommandResult {
                request_id,
                operation: "set_config".to_string(),
                changed: true,
            })
            .await
            .unwrap();

        let session = change.await.unwrap().unwrap();
        session.shutdown().await;
    }

    #[tokio::test]
    async fn stop_is_confirmed_before_shutdown_detaches() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();

        let stop = tokio::spawn(async move { session.stop().await.map(|()| session) });
        let Some(ClientMessage::StopSession { request_id }) = server.recv().await.unwrap() else {
            panic!("expected stop_session");
        };
        assert!(!stop.is_finished());
        server
            .send(&ServerMessage::CommandResult {
                request_id,
                operation: "stop_session".to_string(),
                changed: true,
            })
            .await
            .unwrap();

        let session = stop.await.unwrap().unwrap();
        session.shutdown().await;
        assert_eq!(server.recv().await.unwrap(), Some(ClientMessage::Detach));
    }

    #[tokio::test]
    async fn command_wait_is_bounded_when_daemon_never_answers() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_millis(20)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();

        let change = tokio::spawn(async move {
            session
                .set_config("model", RuntimeValue::String("model-b".to_string()))
                .await
        });
        assert!(matches!(
            server.recv().await.unwrap(),
            Some(ClientMessage::SetConfig { .. })
        ));
        let error = change.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn event_overflow_surfaces_an_actionable_terminal_failure() {
        let (server, transport) = in_memory_transport_pair(32, "daemon");
        let controller = SessionControllerHandle::spawn(
            transport,
            ClientInfo {
                id: "tui".to_string(),
                kind: ClientKind::Terminal,
                label: "terminal test".to_string(),
            },
            vec![ClientCapability::Observe],
            16,
            4,
            1,
        );
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();

        for seq in 1..=4 {
            server
                .send(&ServerMessage::Event {
                    seq,
                    event: SessionEvent::AssistantDelta {
                        text: seq.to_string(),
                    },
                })
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let message = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match session.poll() {
                    Some(TuiSessionUpdate::Failed(message)) => break message,
                    Some(TuiSessionUpdate::Updated) | None => tokio::task::yield_now().await,
                    Some(TuiSessionUpdate::Detached | TuiSessionUpdate::Stopped) => {
                        panic!("termination reason was not surfaced")
                    }
                }
            }
        })
        .await
        .expect("overflow failure should reach the TUI");
        assert!(message.contains("event queue overflowed"));
        session.shutdown().await;
    }

    #[tokio::test]
    async fn usage_response_is_rendered_as_a_frontend_local_notice() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();
        session
            .try_send(SessionControllerCommand::GetUsage)
            .unwrap();
        let Some(ClientMessage::GetUsage { request_id }) = server.recv().await.unwrap() else {
            panic!("expected get_usage");
        };
        server
            .send(&ServerMessage::Usage {
                request_id,
                usage: SessionUsage {
                    input: 10,
                    output: 5,
                    reasoning_output: None,
                    thinking_bytes: 0,
                    cache_read: 4,
                    cache_write: 1,
                    cost_usd_micros: 125_000,
                },
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let _ = session.poll();
                if session
                    .state()
                    .transcript()
                    .iter()
                    .any(|entry| entry.text.contains("cost=$0.1250"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("usage notice should reach the TUI");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn terminal_revocation_surfaces_typed_reason() {
        let (server, controller) = controller();
        let attach =
            tokio::spawn(
                async move { TuiSession::attach(controller, Duration::from_secs(1)).await },
            );
        accept_attach(&server, "daemon-session").await;
        let mut session = attach.await.unwrap().unwrap();
        server
            .send(&ServerMessage::Revoked {
                code: Some(RevocationCode::SessionStopped),
                reason: "stopped".to_string(),
            })
            .await
            .unwrap();
        let update = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(TuiSessionUpdate::Failed(message)) = session.poll() {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(update.contains("SessionStopped"));
        assert!(update.contains("stopped"));
        session.shutdown().await;
    }
}
