//! Thin TUI adapter over the daemon-session controller (Vikunja #1331).
//!
//! The adapter owns no provider, tools, approvals, or persistence. It accepts
//! only canonical daemon snapshots/events and exposes the resulting
//! [`ViewState`](crate::frontend_state::ViewState) to rendering.

use std::time::Duration;

use crate::frontend_state::ViewState;
use crate::session_client::SessionClientOutcome;
use crate::session_controller::{
    ControllerSendError, EpochEvent, SessionControllerCommand, SessionControllerEvent,
    SessionControllerHandle,
};
use crate::session_protocol::RuntimeValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiSessionUpdate {
    Updated,
    Detached,
    Failed(String),
    Stopped,
}

pub struct TuiSession {
    controller: SessionControllerHandle,
    epoch: u64,
    state: ViewState,
    command_timeout: Duration,
    termination_reported: bool,
}

impl TuiSession {
    pub async fn attach(
        mut controller: SessionControllerHandle,
        command_timeout: Duration,
    ) -> Result<Self, anyhow::Error> {
        let attach = async move {
            let epoch = controller.epoch();
            controller
                .send(SessionControllerCommand::Attach { session_id: None })
                .await
                .map_err(|error| anyhow::anyhow!("failed to queue daemon attach: {error:?}"))?;
            loop {
                let event = controller
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("session controller stopped during attach"))?;
                if event.epoch != epoch {
                    continue;
                }
                match event.event {
                    SessionControllerEvent::Attached { state, .. } => {
                        return Ok(Self {
                            controller,
                            epoch,
                            state,
                            command_timeout,
                            termination_reported: false,
                        });
                    }
                    SessionControllerEvent::Failed { message, .. } => {
                        anyhow::bail!("daemon attach failed: {message}");
                    }
                    SessionControllerEvent::Stopped => {
                        anyhow::bail!("session controller stopped during attach");
                    }
                    SessionControllerEvent::StateChanged { .. }
                    | SessionControllerEvent::CommandAccepted { .. }
                    | SessionControllerEvent::Detached => {}
                }
            }
        };
        tokio::time::timeout(command_timeout, attach)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "timed out waiting for daemon attach; increase \
                     session.client_command_timeout_secs if the daemon is healthy but slow"
                )
            })?
    }

    pub fn state(&self) -> &ViewState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut ViewState {
        &mut self.state
    }

    pub fn try_send(&self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
        self.controller.try_send(command)
    }

    pub async fn send(&self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
        self.controller.send(command).await
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
                    .controller
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!(self.controller_stop_message()))?;
                if event.epoch != self.epoch {
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
                        self.state = state;
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
                    SessionControllerEvent::Detached | SessionControllerEvent::Stopped => {
                        anyhow::bail!("{}", self.controller_stop_message());
                    }
                    SessionControllerEvent::Attached { .. } => {
                        anyhow::bail!(
                            "session attachment changed while waiting for {expected_operation}"
                        );
                    }
                    SessionControllerEvent::CommandAccepted { .. }
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
        match self.controller.try_recv() {
            Ok(event) => Some(self.apply(event)),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                if self.termination_reported {
                    Some(TuiSessionUpdate::Stopped)
                } else {
                    self.termination_reported = true;
                    Some(TuiSessionUpdate::Failed(self.controller_stop_message()))
                }
            }
        }
    }

    fn controller_stop_message(&self) -> String {
        self.controller
            .stop_reason()
            .map(|reason| reason.to_string())
            .unwrap_or_else(|| "session controller stopped".to_string())
    }

    fn apply(&mut self, event: EpochEvent) -> TuiSessionUpdate {
        if event.epoch != self.epoch {
            return TuiSessionUpdate::Updated;
        }
        match event.event {
            SessionControllerEvent::Attached { state, .. } => {
                self.state = state;
                TuiSessionUpdate::Updated
            }
            SessionControllerEvent::StateChanged { mut state, outcome } => {
                if let SessionClientOutcome::Usage { usage, .. } = outcome {
                    state.push_system_message(format!(
                        "input={} output={} cache_read={} cache_write={} cost=${:.4}",
                        usage.input,
                        usage.output,
                        usage.cache_read,
                        usage.cache_write,
                        usage.cost_usd_micros as f64 / 1_000_000.0,
                    ));
                }
                self.state = state;
                TuiSessionUpdate::Updated
            }
            SessionControllerEvent::CommandAccepted { .. } => TuiSessionUpdate::Updated,
            SessionControllerEvent::Detached => TuiSessionUpdate::Detached,
            SessionControllerEvent::Failed { operation, message } => {
                TuiSessionUpdate::Failed(format!("{operation}: {message}"))
            }
            SessionControllerEvent::Stopped => TuiSessionUpdate::Stopped,
        }
    }

    pub async fn shutdown(self) {
        self.controller.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_transport::{in_memory_transport_pair, ClientTransport};
    use crate::session_controller::SessionControllerHandle;
    use crate::session_protocol::{
        ClientCapability, ClientInfo, ClientKind, ClientMessage, ContextUsage, ServerMessage,
        SessionEvent, SessionSnapshot, SessionUsage, TurnStatus, PROTOCOL_VERSION,
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
}
