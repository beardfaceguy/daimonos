//! Bounded actor around [`SessionClient`](crate::session_client::SessionClient).
//!
//! A controller owns exactly one physical daemon connection. Every command and
//! outcome is tagged with a monotonically increasing process-local epoch so a
//! frontend can discard late work after reconnecting or switching sessions.

#![allow(dead_code)] // Session switching consumes the remaining commands later.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::client_transport::FrontendTransport;
use crate::frontend_state::ViewState;
use crate::session_client::{SessionClient, SessionClientError, SessionClientOutcome};
use crate::session_protocol::{ApprovalDecision, ClientCapability, ClientInfo, RuntimeValue};

static NEXT_CONNECTION_EPOCH: AtomicU64 = AtomicU64::new(1);

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
    Ping,
    Detach,
    Shutdown,
}

impl SessionControllerCommand {
    fn operation(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::Prompt { .. } => "prompt",
            Self::Interrupt => "interrupt",
            Self::Approve { .. } => "approve",
            Self::SetConfig { .. } => "set_config",
            Self::StopSession => "stop_session",
            Self::Ping => "ping",
            Self::Detach => "detach",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionControllerEvent {
    Attached {
        state: ViewState,
        granted_capabilities: Vec<ClientCapability>,
    },
    StateChanged {
        state: ViewState,
        outcome: SessionClientOutcome,
    },
    CommandAccepted {
        operation: &'static str,
        request_id: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerStopReason {
    Shutdown,
    CommandChannelClosed,
    EventQueueOverflow,
    Transport(String),
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
        mut client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
        max_scrollback_entries: usize,
        command_capacity: usize,
        event_capacity: usize,
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

fn emit(sender: &mpsc::Sender<EpochEvent>, epoch: u64, event: SessionControllerEvent) -> bool {
    sender.try_send(EpochEvent { epoch, event }).is_ok()
}

async fn run_controller<T: FrontendTransport>(
    epoch: u64,
    mut client: SessionClient<T>,
    mut commands: mpsc::Receiver<SessionControllerCommand>,
    events: mpsc::Sender<EpochEvent>,
    stop_reason: Arc<StdMutex<Option<ControllerStopReason>>>,
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
                        Ok(outcome) => {
                            let state = client.state().clone();
                            if !emit(&events, epoch, SessionControllerEvent::StateChanged { state, outcome }) {
                                break 'controller ControllerStopReason::EventQueueOverflow;
                            }
                        }
                        Err(error) => {
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
        Err(error) => emit_failure(events, epoch, operation, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_transport::{in_memory_transport_pair, ClientTransport};
    use crate::session_protocol::{
        ClientKind, ClientMessage, ContextUsage, ServerMessage, SessionEvent, SessionSnapshot,
        TurnStatus, PROTOCOL_VERSION,
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
