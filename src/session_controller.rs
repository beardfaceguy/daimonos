//! Bounded actor around [`SessionClient`](crate::session_client::SessionClient).
//!
//! A controller owns exactly one physical daemon connection. Every command and
//! outcome is tagged with a monotonically increasing process-local epoch so a
//! frontend can discard late work after reconnecting or switching sessions.

#![allow(dead_code)] // Session switching consumes the remaining commands later.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::client_transport::FrontendTransport;
use crate::frontend_state::ViewState;
use crate::session_client::{SessionClient, SessionClientError, SessionClientOutcome};
use crate::session_protocol::{
    ApprovalDecision, AttachDeniedCode, ClientCapability, ClientInfo, RevocationCode, RuntimeValue,
};

static NEXT_CONNECTION_EPOCH: AtomicU64 = AtomicU64::new(1);

pub type ReconnectFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, SessionClientError>> + Send + 'static>>;
pub type ReconnectFactory<T> = Arc<dyn Fn() -> ReconnectFuture<T> + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub attempts: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionControllerCommand {
    Attach {
        session_id: Option<String>,
    },
    Prompt {
        text: String,
    },
    Interrupt,
    Approve {
        approval_id: String,
        decision: ApprovalDecision,
    },
    SetConfig {
        config_id: String,
        value: RuntimeValue,
    },
    StopSession,
    ClearHistory,
    GetUsage,
    ListSessions {
        cursor: Option<String>,
    },
    Sync,
    Ping,
    Detach,
    Shutdown,
}

impl SessionControllerCommand {
    pub(crate) fn operation(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::Prompt { .. } => "prompt",
            Self::Interrupt => "interrupt",
            Self::Approve { .. } => "approve",
            Self::SetConfig { .. } => "set_config",
            Self::StopSession => "stop_session",
            Self::ClearHistory => "clear_history",
            Self::GetUsage => "get_usage",
            Self::ListSessions { .. } => "list_sessions",
            Self::Sync => "sync",
            Self::Ping => "ping",
            Self::Detach => "detach",
            Self::Shutdown => "shutdown",
        }
    }

    pub(crate) fn blocks_switch(&self) -> bool {
        matches!(
            self,
            Self::Prompt { .. }
                | Self::Interrupt
                | Self::Approve { .. }
                | Self::SetConfig { .. }
                | Self::StopSession
                | Self::ClearHistory
                | Self::ListSessions { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionControllerEvent {
    Attached {
        state: ViewState,
        granted_capabilities: Vec<ClientCapability>,
    },
    Reconnected {
        state: ViewState,
        granted_capabilities: Vec<ClientCapability>,
        message: String,
    },
    StateChanged {
        state: ViewState,
        outcome: SessionClientOutcome,
    },
    CommandAccepted {
        operation: &'static str,
        request_id: Option<String>,
    },
    CommandRejected {
        operation: &'static str,
        message: String,
    },
    AttachFailed {
        code: Option<AttachDeniedCode>,
        message: String,
    },
    Detached,
    Failed {
        operation: &'static str,
        message: String,
    },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochEvent {
    pub epoch: u64,
    pub event: SessionControllerEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerSendError {
    Backpressure,
    Closed,
    SwitchInProgress,
    OperationInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerStopReason {
    Shutdown,
    CommandChannelClosed,
    EventQueueOverflow,
    Transport(String),
    Revoked(String),
    ReconnectExhausted(String),
}

impl std::fmt::Display for ControllerStopReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shutdown => formatter.write_str("controller shut down"),
            Self::CommandChannelClosed => formatter.write_str("controller command channel closed"),
            Self::EventQueueOverflow => formatter.write_str(
                "controller event queue overflowed; daemon session remains available to reattach",
            ),
            Self::Transport(message) => write!(formatter, "daemon transport failed: {message}"),
            Self::Revoked(message) => write!(formatter, "daemon revoked the session: {message}"),
            Self::ReconnectExhausted(message) => {
                write!(formatter, "daemon reconnect attempts exhausted: {message}")
            }
        }
    }
}

pub struct SessionControllerHandle {
    epoch: u64,
    commands: mpsc::Sender<SessionControllerCommand>,
    events: mpsc::Receiver<EpochEvent>,
    task: JoinHandle<()>,
    stop_reason: Arc<StdMutex<Option<ControllerStopReason>>>,
}

impl SessionControllerHandle {
    pub fn spawn<T: FrontendTransport + 'static>(
        transport: T,
        client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
        max_scrollback_entries: usize,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Self {
        Self::spawn_inner(
            transport,
            client,
            requested_capabilities,
            max_scrollback_entries,
            command_capacity,
            event_capacity,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_reconnect<T: FrontendTransport + 'static>(
        transport: T,
        client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
        max_scrollback_entries: usize,
        command_capacity: usize,
        event_capacity: usize,
        reconnect_policy: ReconnectPolicy,
        reconnect_factory: ReconnectFactory<T>,
    ) -> Self {
        Self::spawn_inner(
            transport,
            client,
            requested_capabilities,
            max_scrollback_entries,
            command_capacity,
            event_capacity,
            Some((reconnect_policy, reconnect_factory)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_inner<T: FrontendTransport + 'static>(
        transport: T,
        mut client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
        max_scrollback_entries: usize,
        command_capacity: usize,
        event_capacity: usize,
        reconnect: Option<(ReconnectPolicy, ReconnectFactory<T>)>,
    ) -> Self {
        let epoch = NEXT_CONNECTION_EPOCH.fetch_add(1, Ordering::Relaxed);
        client.id = format!("{}-{}-{epoch}", client.id, std::process::id());
        let client = SessionClient::new(
            transport,
            client,
            requested_capabilities,
            max_scrollback_entries,
        );
        let (command_tx, command_rx) = mpsc::channel(command_capacity.max(1));
        let (event_tx, event_rx) = mpsc::channel(event_capacity.max(1));
        let stop_reason = Arc::new(StdMutex::new(None));
        let task = tokio::spawn(run_controller(
            epoch,
            client,
            command_rx,
            event_tx,
            Arc::clone(&stop_reason),
            reconnect,
        ));
        Self {
            epoch,
            commands: command_tx,
            events: event_rx,
            task,
            stop_reason,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub async fn send(&self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| ControllerSendError::Closed)
    }

    pub fn try_send(&self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ControllerSendError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => ControllerSendError::Closed,
            })
    }

    pub async fn recv(&mut self) -> Option<EpochEvent> {
        self.events.recv().await
    }

    pub fn try_recv(&mut self) -> Result<EpochEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }

    pub fn stop_reason(&self) -> Option<ControllerStopReason> {
        self.stop_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn shutdown(mut self) {
        let _ = self.commands.send(SessionControllerCommand::Shutdown).await;
        let _ = (&mut self.task).await;
    }
}

impl Drop for SessionControllerHandle {
    fn drop(&mut self) {
        // The handle is the sole owner of this physical connection. Aborting
        // makes cancellation of an in-progress attach drop the transport
        // promptly instead of leaving the actor blocked inside the handshake.
        if !self.task.is_finished() {
            tracing::debug!(
                target: "daimonos::session_controller",
                event = "controller_handle_drop_abort",
                epoch = self.epoch,
            );
            self.task.abort();
        }
    }
}

fn emit(sender: &mpsc::Sender<EpochEvent>, epoch: u64, event: SessionControllerEvent) -> bool {
    sender.try_send(EpochEvent { epoch, event }).is_ok()
}

fn reconnected_event<T: FrontendTransport>(
    client: &SessionClient<T>,
    message: impl Into<String>,
) -> SessionControllerEvent {
    SessionControllerEvent::Reconnected {
        state: client.state().clone(),
        granted_capabilities: client.granted_capabilities().to_vec(),
        message: message.into(),
    }
}

async fn reconnect_client<T: FrontendTransport>(
    client: &mut SessionClient<T>,
    policy: ReconnectPolicy,
    factory: &ReconnectFactory<T>,
) -> Result<(), SessionClientError> {
    let session_id = client.state().session_id().to_string();
    let mut delay = policy.initial_backoff;
    let mut last_error = "no reconnect attempt completed".to_string();
    for attempt in 0..policy.attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
        }
        match factory().await {
            Ok(transport) => {
                client.replace_transport(transport);
                client.rotate_client_identity();
                match client.attach(Some(session_id.clone())).await {
                    Ok(()) => return Ok(()),
                    Err(error) => last_error = error.to_string(),
                }
            }
            Err(error) => last_error = error.to_string(),
        }
        if attempt > 0 {
            delay = delay.saturating_mul(2).min(policy.max_backoff);
        }
    }
    Err(SessionClientError::Protocol(format!(
        "reconnect exhausted: {last_error}"
    )))
}

enum ReconnectDrive {
    Connected,
    Stop(ControllerStopReason),
    Failed(SessionClientError),
}

async fn drive_reconnect<T: FrontendTransport>(
    client: &mut SessionClient<T>,
    policy: ReconnectPolicy,
    factory: &ReconnectFactory<T>,
    commands: &mut mpsc::Receiver<SessionControllerCommand>,
    events: &mpsc::Sender<EpochEvent>,
    epoch: u64,
) -> ReconnectDrive {
    let reconnect = reconnect_client(client, policy, factory);
    tokio::pin!(reconnect);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => {
                let Some(command) = command else {
                    return ReconnectDrive::Stop(ControllerStopReason::CommandChannelClosed);
                };
                match command {
                    SessionControllerCommand::Shutdown => {
                        return ReconnectDrive::Stop(ControllerStopReason::Shutdown);
                    }
                    SessionControllerCommand::Detach => {
                        if !emit(events, epoch, SessionControllerEvent::Detached) {
                            return ReconnectDrive::Stop(ControllerStopReason::EventQueueOverflow);
                        }
                        return ReconnectDrive::Stop(ControllerStopReason::Shutdown);
                    }
                    command => {
                        // Every controller command is daemon-bound today.
                        // Frontend-local input (scroll/composer editing) never
                        // enters this channel, so blanket rejection is honest
                        // while canonical state is being recovered.
                        if !emit(
                            events,
                            epoch,
                            SessionControllerEvent::CommandRejected {
                                operation: command.operation(),
                                message: "command rejected while reconnecting".to_string(),
                            },
                        ) {
                            return ReconnectDrive::Stop(ControllerStopReason::EventQueueOverflow);
                        }
                    }
                }
            }
            result = &mut reconnect => {
                return match result {
                    Ok(()) => ReconnectDrive::Connected,
                    Err(error) => ReconnectDrive::Failed(error),
                };
            }
        }
    }
}

async fn run_controller<T: FrontendTransport>(
    epoch: u64,
    mut client: SessionClient<T>,
    mut commands: mpsc::Receiver<SessionControllerCommand>,
    events: mpsc::Sender<EpochEvent>,
    stop_reason: Arc<StdMutex<Option<ControllerStopReason>>>,
    reconnect: Option<(ReconnectPolicy, ReconnectFactory<T>)>,
) {
    let reason = 'controller: loop {
        if client.is_attached() {
            tokio::select! {
                biased;
                command = commands.recv() => {
                    let Some(command) = command else {
                        break 'controller ControllerStopReason::CommandChannelClosed;
                    };
                    let shutdown = matches!(command, SessionControllerCommand::Shutdown);
                    if !handle_command(epoch, &mut client, command, &events).await {
                        break 'controller if shutdown {
                            ControllerStopReason::Shutdown
                        } else {
                            ControllerStopReason::EventQueueOverflow
                        };
                    }
                }
                received = client.receive() => {
                    match received {
                        Ok(SessionClientOutcome::Revoked {
                            code: Some(RevocationCode::EventQueueLagged),
                            ..
                        }) if reconnect.is_some() => {
                            let (policy, factory) = reconnect.as_ref().expect("checked reconnect");
                            match drive_reconnect(
                                &mut client,
                                *policy,
                                factory,
                                &mut commands,
                                &events,
                                epoch,
                            )
                            .await
                            {
                                ReconnectDrive::Connected => {
                                    if !emit(
                                        &events,
                                        epoch,
                                        reconnected_event(
                                            &client,
                                            "session resumed after event queue lag; verify any command accepted immediately before revocation",
                                        ),
                                    ) {
                                        break 'controller ControllerStopReason::EventQueueOverflow;
                                    }
                                }
                                ReconnectDrive::Stop(reason) => break 'controller reason,
                                ReconnectDrive::Failed(error) => {
                                    let message = error.to_string();
                                    let _ = emit_failure(&events, epoch, "reconnect", &error);
                                    break 'controller ControllerStopReason::ReconnectExhausted(message);
                                }
                            }
                        }
                        Ok(SessionClientOutcome::Revoked { code, reason }) => {
                            let stop_message = reason.clone();
                            let outcome = SessionClientOutcome::Revoked { code, reason };
                            let state = client.state().clone();
                            if !emit(&events, epoch, SessionControllerEvent::StateChanged { state, outcome }) {
                                break 'controller ControllerStopReason::EventQueueOverflow;
                            }
                            break 'controller ControllerStopReason::Revoked(stop_message);
                        }
                        Ok(outcome) => {
                            let state = client.state().clone();
                            if !emit(&events, epoch, SessionControllerEvent::StateChanged { state, outcome }) {
                                break 'controller ControllerStopReason::EventQueueOverflow;
                            }
                        }
                        Err(error) => {
                            if let Some((policy, factory)) = reconnect.as_ref() {
                                match drive_reconnect(
                                    &mut client,
                                    *policy,
                                    factory,
                                    &mut commands,
                                    &events,
                                    epoch,
                                )
                                .await
                                {
                                    ReconnectDrive::Connected => {
                                        if !emit(
                                            &events,
                                            epoch,
                                            reconnected_event(
                                                &client,
                                                "session resumed after transport loss; verify any command accepted immediately before disconnect",
                                            ),
                                        ) {
                                            break 'controller ControllerStopReason::EventQueueOverflow;
                                        }
                                        continue;
                                    }
                                    ReconnectDrive::Stop(reason) => break 'controller reason,
                                    ReconnectDrive::Failed(reconnect_error) => {
                                        let message = reconnect_error.to_string();
                                        let _ = emit_failure(
                                            &events,
                                            epoch,
                                            "reconnect",
                                            &reconnect_error,
                                        );
                                        break 'controller ControllerStopReason::ReconnectExhausted(message);
                                    }
                                }
                            }
                            let _ = emit_failure(&events, epoch, "receive", &error);
                            break 'controller ControllerStopReason::Transport(error.to_string());
                        }
                    }
                }
            }
        } else {
            let Some(command) = commands.recv().await else {
                break 'controller ControllerStopReason::CommandChannelClosed;
            };
            let shutdown = matches!(command, SessionControllerCommand::Shutdown);
            if !handle_command(epoch, &mut client, command, &events).await {
                break 'controller if shutdown {
                    ControllerStopReason::Shutdown
                } else {
                    ControllerStopReason::EventQueueOverflow
                };
            }
        }
    };
    *stop_reason
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
    let _ = emit(&events, epoch, SessionControllerEvent::Stopped);
}

fn emit_failure(
    events: &mpsc::Sender<EpochEvent>,
    epoch: u64,
    operation: &'static str,
    error: &SessionClientError,
) -> bool {
    emit(
        events,
        epoch,
        SessionControllerEvent::Failed {
            operation,
            message: error.to_string(),
        },
    )
}

async fn handle_command<T: FrontendTransport>(
    epoch: u64,
    client: &mut SessionClient<T>,
    command: SessionControllerCommand,
    events: &mpsc::Sender<EpochEvent>,
) -> bool {
    let operation = command.operation();
    let result: Result<(Option<String>, Option<SessionControllerEvent>), SessionClientError> =
        match command {
            SessionControllerCommand::Attach { session_id } => {
                client.attach(session_id).await.map(|()| {
                    (
                        None,
                        Some(SessionControllerEvent::Attached {
                            state: client.state().clone(),
                            granted_capabilities: client.granted_capabilities().to_vec(),
                        }),
                    )
                })
            }
            SessionControllerCommand::Prompt { text } => {
                client.prompt(text).await.map(|id| (Some(id), None))
            }
            SessionControllerCommand::Interrupt => {
                client.interrupt().await.map(|id| (Some(id), None))
            }
            SessionControllerCommand::Approve {
                approval_id,
                decision,
            } => client
                .approve(approval_id, decision)
                .await
                .map(|()| (None, None)),
            SessionControllerCommand::SetConfig { config_id, value } => client
                .set_config(config_id, value)
                .await
                .map(|id| (Some(id), None)),
            SessionControllerCommand::StopSession => {
                client.stop_session().await.map(|id| (Some(id), None))
            }
            SessionControllerCommand::ClearHistory => {
                client.clear_history().await.map(|id| (Some(id), None))
            }
            SessionControllerCommand::GetUsage => {
                client.get_usage().await.map(|id| (Some(id), None))
            }
            SessionControllerCommand::ListSessions { cursor } => client
                .list_sessions(cursor)
                .await
                .map(|id| (Some(id), None)),
            SessionControllerCommand::Sync => client.sync().await.map(|()| (None, None)),
            SessionControllerCommand::Ping => client.ping().await.map(|()| (None, None)),
            SessionControllerCommand::Detach => client
                .detach()
                .await
                .map(|()| (None, Some(SessionControllerEvent::Detached))),
            SessionControllerCommand::Shutdown => {
                if client.is_attached() {
                    let _ = client.detach().await;
                }
                return false;
            }
        };

    match result {
        Ok((request_id, event)) => {
            let event = event.unwrap_or(SessionControllerEvent::CommandAccepted {
                operation,
                request_id,
            });
            emit(events, epoch, event)
        }
        Err(SessionClientError::AttachDenied { code, reason }) if operation == "attach" => emit(
            events,
            epoch,
            SessionControllerEvent::AttachFailed {
                code,
                message: reason,
            },
        ),
        Err(error) => emit_failure(events, epoch, operation, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use crate::client_transport::{in_memory_transport_pair, ClientTransport, InMemoryClient};
    use crate::session_protocol::{
        ClientKind, ClientMessage, ContextUsage, RevocationCode, ServerMessage, SessionEvent,
        SessionSnapshot, TranscriptEntry, TranscriptRole, TurnStatus, PROTOCOL_VERSION,
    };

    fn info(id: &str) -> ClientInfo {
        ClientInfo {
            id: id.to_string(),
            kind: ClientKind::Headless,
            label: "controller test".to_string(),
        }
    }

    fn snapshot(session_id: &str, seq: u64) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.to_string(),
            seq,
            turn_status: TurnStatus::Idle,
            transcript: Vec::new(),
            tool_calls: Vec::new(),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: Some(ContextUsage::new(0, Some(100), 0, None)),
            history_truncated: false,
        }
    }

    async fn attach_server(server: &impl ClientTransport, session_id: &str) -> ClientInfo {
        let Some(ClientMessage::Attach { client, .. }) = server.recv().await.unwrap() else {
            panic!("expected attach");
        };
        server
            .send(&ServerMessage::AttachOk {
                protocol_version: PROTOCOL_VERSION,
                session_id: session_id.to_string(),
                granted_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
                seq: 0,
            })
            .await
            .unwrap();
        server
            .send(&ServerMessage::Snapshot {
                seq: 0,
                state: snapshot(session_id, 0),
            })
            .await
            .unwrap();
        client
    }

    #[tokio::test]
    async fn assigns_unique_connection_epochs_and_client_ids() {
        let (_server_a, transport_a) = in_memory_transport_pair(1, "daemon-a");
        let (_server_b, transport_b) = in_memory_transport_pair(1, "daemon-b");
        let a = SessionControllerHandle::spawn(transport_a, info("ui"), vec![], 10, 1, 1);
        let b = SessionControllerHandle::spawn(transport_b, info("ui"), vec![], 10, 1, 1);
        assert_ne!(a.epoch(), b.epoch());
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn attaches_and_tags_snapshot_with_epoch() {
        let (server, transport) = in_memory_transport_pair(4, "daemon");
        let mut controller = SessionControllerHandle::spawn(
            transport,
            info("ui"),
            vec![ClientCapability::Observe, ClientCapability::Prompt],
            10,
            2,
            2,
        );
        let epoch = controller.epoch();
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        let attached_client = attach_server(&server, "session-1").await;
        assert!(attached_client.id.starts_with("ui-"));
        assert!(attached_client.id.ends_with(&epoch.to_string()));
        let event = controller.recv().await.unwrap();
        assert_eq!(event.epoch, epoch);
        let SessionControllerEvent::Attached { state, .. } = event.event else {
            panic!("expected attached event");
        };
        assert_eq!(state.session_id(), "session-1");
        controller.shutdown().await;
    }

    #[tokio::test]
    async fn lag_revocation_resumes_with_watermark_on_new_transport() {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let (reconnect_server, reconnect_transport) =
            in_memory_transport_pair(8, "daemon-reconnect");
        let transports = Arc::new(StdMutex::new(VecDeque::<InMemoryClient>::from([
            reconnect_transport,
        ])));
        let reconnect_factory: ReconnectFactory<InMemoryClient> = {
            let transports = Arc::clone(&transports);
            Arc::new(move || {
                let transport = transports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front()
                    .ok_or_else(|| {
                        SessionClientError::Protocol("no reconnect transport".to_string())
                    });
                Box::pin(async move { transport })
            })
        };
        let mut controller = SessionControllerHandle::spawn_with_reconnect(
            transport,
            info("ui"),
            vec![ClientCapability::Observe, ClientCapability::Prompt],
            10,
            4,
            8,
            ReconnectPolicy {
                attempts: 1,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
            },
            reconnect_factory,
        );
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        let original_client = attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();

        server
            .send(&ServerMessage::Revoked {
                code: Some(RevocationCode::EventQueueLagged),
                reason: "lagged".to_string(),
            })
            .await
            .unwrap();
        let Some(ClientMessage::Resume {
            session_id,
            last_seen_seq,
            client,
            ..
        }) = reconnect_server.recv().await.unwrap()
        else {
            panic!("expected resume");
        };
        assert_eq!(session_id, "session-1");
        assert_eq!(last_seen_seq, 0);
        assert_ne!(client.id, original_client.id);
        reconnect_server
            .send(&ServerMessage::AttachOk {
                protocol_version: PROTOCOL_VERSION,
                session_id: session_id.clone(),
                granted_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
                seq: 5,
            })
            .await
            .unwrap();
        let mut recovered = snapshot(&session_id, 5);
        recovered.transcript.push(TranscriptEntry {
            id: 7,
            role: TranscriptRole::Assistant,
            text: "recovered snapshot".to_string(),
            outcome: None,
        });
        reconnect_server
            .send(&ServerMessage::Snapshot {
                seq: 5,
                state: recovered,
            })
            .await
            .unwrap();
        let SessionControllerEvent::Reconnected { state, .. } =
            controller.recv().await.unwrap().event
        else {
            panic!("expected reconnected event");
        };
        assert_eq!(state.last_seq(), 5);
        assert_eq!(state.transcript()[0].text, "recovered snapshot");

        controller.shutdown().await;
        assert_eq!(
            reconnect_server.recv().await.unwrap(),
            Some(ClientMessage::Detach)
        );
    }

    #[tokio::test]
    async fn transport_loss_reconnects_instead_of_stopping_controller() {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let (reconnect_server, reconnect_transport) =
            in_memory_transport_pair(8, "daemon-reconnect");
        let transports = Arc::new(StdMutex::new(VecDeque::<InMemoryClient>::from([
            reconnect_transport,
        ])));
        let reconnect_factory: ReconnectFactory<InMemoryClient> = {
            let transports = Arc::clone(&transports);
            Arc::new(move || {
                let transport = transports
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front()
                    .ok_or_else(|| {
                        SessionClientError::Protocol("no reconnect transport".to_string())
                    });
                Box::pin(async move { transport })
            })
        };
        let mut controller = SessionControllerHandle::spawn_with_reconnect(
            transport,
            info("ui"),
            vec![ClientCapability::Observe],
            10,
            4,
            8,
            ReconnectPolicy {
                attempts: 1,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
            },
            reconnect_factory,
        );
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        let original_client = attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();
        drop(server);

        let Some(ClientMessage::Resume {
            session_id,
            last_seen_seq,
            client,
            ..
        }) = reconnect_server.recv().await.unwrap()
        else {
            panic!("expected resume after transport loss");
        };
        assert_eq!(last_seen_seq, 0);
        assert_ne!(client.id, original_client.id);
        reconnect_server
            .send(&ServerMessage::AttachOk {
                protocol_version: PROTOCOL_VERSION,
                session_id,
                granted_capabilities: vec![ClientCapability::Observe],
                seq: 0,
            })
            .await
            .unwrap();
        assert!(matches!(
            controller.recv().await.unwrap().event,
            SessionControllerEvent::Reconnected { .. }
        ));
        controller.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_an_inflight_reconnect_promptly() {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let entered = Arc::new(tokio::sync::Notify::new());
        let reconnect_factory: ReconnectFactory<InMemoryClient> = {
            let entered = Arc::clone(&entered);
            Arc::new(move || {
                let entered = Arc::clone(&entered);
                Box::pin(async move {
                    entered.notify_one();
                    std::future::pending::<Result<InMemoryClient, SessionClientError>>().await
                })
            })
        };
        let mut controller = SessionControllerHandle::spawn_with_reconnect(
            transport,
            info("ui"),
            vec![ClientCapability::Observe],
            10,
            1,
            4,
            ReconnectPolicy {
                attempts: 4,
                initial_backoff: Duration::from_secs(1),
                max_backoff: Duration::from_secs(1),
            },
            reconnect_factory,
        );
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();
        drop(server);
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("controller entered reconnect factory");
        controller
            .send(SessionControllerCommand::Ping)
            .await
            .unwrap();
        assert!(matches!(
            controller.recv().await.unwrap().event,
            SessionControllerEvent::CommandRejected {
                operation: "ping",
                ..
            }
        ));
        tokio::time::timeout(Duration::from_millis(100), controller.shutdown())
            .await
            .expect("shutdown cancels reconnect instead of waiting for retries");
    }

    #[tokio::test]
    async fn attachment_replaced_revocation_never_enters_reconnect_loop() {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let reconnect_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reconnect_factory: ReconnectFactory<InMemoryClient> = {
            let reconnect_calls = Arc::clone(&reconnect_calls);
            Arc::new(move || {
                reconnect_calls.fetch_add(1, Ordering::Relaxed);
                Box::pin(async {
                    Err(SessionClientError::Protocol(
                        "unexpected reconnect".to_string(),
                    ))
                })
            })
        };
        let mut controller = SessionControllerHandle::spawn_with_reconnect(
            transport,
            info("ui"),
            vec![ClientCapability::Observe],
            10,
            2,
            4,
            ReconnectPolicy {
                attempts: 1,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
            },
            reconnect_factory,
        );
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();
        server
            .send(&ServerMessage::Revoked {
                code: Some(RevocationCode::AttachmentReplaced),
                reason: "replaced".to_string(),
            })
            .await
            .unwrap();
        drop(server);
        assert!(matches!(
            controller.recv().await.unwrap().event,
            SessionControllerEvent::StateChanged {
                outcome: SessionClientOutcome::Revoked {
                    code: Some(RevocationCode::AttachmentReplaced),
                    ..
                },
                ..
            }
        ));
        let _ = controller.recv().await;
        assert_eq!(reconnect_calls.load(Ordering::Relaxed), 0);
        controller.shutdown().await;
    }

    #[tokio::test]
    async fn forwards_commands_after_attachment() {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let mut controller = SessionControllerHandle::spawn(
            transport,
            info("ui"),
            vec![ClientCapability::Observe, ClientCapability::Prompt],
            10,
            4,
            4,
        );
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();
        controller
            .send(SessionControllerCommand::Prompt { text: "hi".into() })
            .await
            .unwrap();
        let Some(ClientMessage::Prompt { text, .. }) = server.recv().await.unwrap() else {
            panic!("expected prompt");
        };
        assert_eq!(text, "hi");
        let accepted = controller.recv().await.unwrap();
        assert!(matches!(
            accepted.event,
            SessionControllerEvent::CommandAccepted {
                operation: "prompt",
                request_id: Some(_)
            }
        ));
        controller.shutdown().await;
    }

    #[tokio::test]
    async fn try_send_reports_bounded_backpressure() {
        let (server, transport) = in_memory_transport_pair(1, "daemon");
        let controller = SessionControllerHandle::spawn(transport, info("ui"), vec![], 10, 1, 1);
        controller.try_send(SessionControllerCommand::Ping).unwrap();
        let result = controller.try_send(SessionControllerCommand::Ping);
        assert!(matches!(
            result,
            Err(ControllerSendError::Backpressure) | Ok(())
        ));
        // The actor may win the race and drain the first command. Either way,
        // channel capacity remains bounded. Closing the peer also proves a
        // command blocked on transport teardown cannot leak the actor.
        drop(server);
        tokio::time::timeout(std::time::Duration::from_secs(1), controller.shutdown())
            .await
            .expect("controller should stop when its peer closes");
    }

    #[tokio::test]
    async fn disconnect_emits_epoch_tagged_failure() {
        let (server, transport) = in_memory_transport_pair(4, "daemon");
        let mut controller = SessionControllerHandle::spawn(
            transport,
            info("ui"),
            vec![ClientCapability::Observe],
            10,
            2,
            2,
        );
        let epoch = controller.epoch();
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();
        drop(server);
        let event = controller.recv().await.unwrap();
        assert_eq!(event.epoch, epoch);
        assert!(matches!(
            event.event,
            SessionControllerEvent::Failed {
                operation: "receive",
                ..
            }
        ));
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), controller.recv())
            .await
            .expect("controller should stop after transport failure")
            .expect("stopped event");
        assert!(matches!(stopped.event, SessionControllerEvent::Stopped));
        controller.shutdown().await;
    }

    #[tokio::test]
    async fn event_overflow_records_a_durable_stop_reason() {
        let (server, transport) = in_memory_transport_pair(8, "daemon");
        let mut controller = SessionControllerHandle::spawn(
            transport,
            info("ui"),
            vec![ClientCapability::Observe],
            10,
            2,
            1,
        );
        controller
            .send(SessionControllerCommand::Attach { session_id: None })
            .await
            .unwrap();
        attach_server(&server, "session-1").await;
        let _ = controller.recv().await.unwrap();

        server
            .send(&ServerMessage::Event {
                seq: 1,
                event: SessionEvent::UserMessage {
                    text: "one".to_string(),
                    request_id: None,
                },
            })
            .await
            .unwrap();
        server
            .send(&ServerMessage::Event {
                seq: 2,
                event: SessionEvent::AssistantDelta {
                    text: "two".to_string(),
                },
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while controller.stop_reason().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controller should stop when its event queue overflows");
        let _ = controller.recv().await.unwrap();
        assert!(controller.recv().await.is_none());
        assert_eq!(
            controller.stop_reason(),
            Some(ControllerStopReason::EventQueueOverflow)
        );
        controller.shutdown().await;
    }
}
