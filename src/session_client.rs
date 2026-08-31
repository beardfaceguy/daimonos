//! Reusable transport-neutral client for one daemon-owned session.
//!
//! This is the synchronous command/receive core used by frontend adapters.
//! [`SessionController`](crate::session_controller::SessionControllerHandle)
//! supplies bounded actor/channel orchestration for the TUI.
#![allow(dead_code)] // Discovery/reconnect actions land in later task-1331 slices.

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;

use crate::client_transport::{FrontendTransport, TransportError};
use crate::frontend_state::{ApplyOutcome, ViewState};
use crate::session_protocol::{
    has_capability, ApprovalDecision, AttachDeniedCode, ClientCapability, ClientInfo,
    ClientMessage, RevocationCode, RuntimeValue, ServerMessage, SessionListEntry, SessionSnapshot,
    SessionUsage, SessionWorkspace, PROTOCOL_VERSION,
};

#[derive(Debug)]
pub enum SessionClientError {
    Transport(TransportError),
    Disconnected,
    AttachDenied {
        code: Option<AttachDeniedCode>,
        reason: String,
    },
    Server {
        request_id: Option<String>,
        code: String,
        message: String,
    },
    Protocol(String),
    MissingCapability(ClientCapability),
}

impl SessionClientError {
    /// Return the rejecting transport endpoint's configured frame bound.
    pub fn frame_limit_mismatch(&self) -> Option<usize> {
        match self {
            Self::Transport(TransportError::FrameTooLarge { max_bytes }) => Some(*max_bytes),
            _ => None,
        }
    }
}

impl fmt::Display for SessionClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Attach, receive, and command transport failures all pass through
            // this variant, while callers can classify it without parsing text.
            Self::Transport(TransportError::FrameTooLarge { max_bytes }) => write!(
                formatter,
                "frame_limit_mismatch: transport frame exceeded configured max_frame_bytes={max_bytes}; \
                 align session.max_frame_bytes between daemon and client"
            ),
            Self::Transport(error) => write!(formatter, "{error}"),
            Self::Disconnected => formatter.write_str("frontend transport disconnected"),
            Self::AttachDenied { code, reason } => {
                write!(formatter, "attach denied ({code:?}): {reason}")
            }
            Self::Server { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::MissingCapability(capability) => {
                write!(formatter, "capability not granted: {capability:?}")
            }
        }
    }
}

impl std::error::Error for SessionClientError {}

impl From<TransportError> for SessionClientError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionClientOutcome {
    AppliedEvent(u64),
    IgnoredDuplicate(u64),
    RequestedSync {
        expected_seq: u64,
    },
    AppliedSnapshot(u64),
    Pong,
    CommandResult {
        request_id: String,
        operation: String,
        changed: bool,
    },
    SessionList {
        request_id: String,
        workspace: Option<SessionWorkspace>,
        sessions: Vec<SessionListEntry>,
        next_cursor: Option<String>,
        incomplete: bool,
    },
    Usage {
        request_id: String,
        usage: SessionUsage,
    },
    ServerError {
        request_id: Option<String>,
        code: String,
        message: String,
    },
    Revoked {
        code: Option<RevocationCode>,
        reason: String,
    },
}

fn validate_snapshot_ids(snapshot: &SessionSnapshot) -> Result<(), SessionClientError> {
    fn require_unique<T: Clone + Eq + Hash + fmt::Debug>(
        kind: &str,
        ids: impl IntoIterator<Item = T>,
    ) -> Result<(), SessionClientError> {
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                return Err(SessionClientError::Protocol(format!(
                    "snapshot contains duplicate {kind} id {id:?}"
                )));
            }
        }
        Ok(())
    }

    require_unique("timeline", snapshot.timeline.iter().map(|entry| entry.id))?;
    require_unique(
        "active tool occurrence",
        snapshot.active_tools.iter().map(|tool| tool.occurrence_id),
    )?;
    require_unique(
        "approval",
        snapshot
            .pending_approvals
            .iter()
            .map(|approval| approval.id.as_str()),
    )?;
    if snapshot.history_window.retained != snapshot.timeline.len()
        || snapshot.history_window.total
            != Some(
                snapshot
                    .history_window
                    .truncated_before
                    .saturating_add(snapshot.timeline.len() as u64),
            )
    {
        return Err(SessionClientError::Protocol(
            "snapshot history window does not match timeline".to_string(),
        ));
    }
    if snapshot
        .active_tools
        .iter()
        .any(|tool| tool.status.is_terminal())
    {
        return Err(SessionClientError::Protocol(
            "snapshot active_tools contains terminal tool".to_string(),
        ));
    }
    if snapshot.pending_approvals.iter().any(|approval| {
        !snapshot
            .active_tools
            .iter()
            .any(|tool| tool.tool_call_id == approval.tool_call_id)
    }) {
        return Err(SessionClientError::Protocol(
            "snapshot approval references missing active tool".to_string(),
        ));
    }
    require_unique(
        "runtime option",
        snapshot
            .runtime_options
            .iter()
            .map(|option| option.id.as_str()),
    )
}

pub struct SessionClient<T: FrontendTransport> {
    transport: T,
    state: ViewState,
    client: ClientInfo,
    requested_capabilities: Vec<ClientCapability>,
    granted_capabilities: Vec<ClientCapability>,
    attached: bool,
}

impl<T: FrontendTransport> SessionClient<T> {
    pub fn new(
        transport: T,
        client: ClientInfo,
        requested_capabilities: Vec<ClientCapability>,
        max_scrollback_entries: usize,
    ) -> Self {
        Self {
            transport,
            state: ViewState::with_scrollback_limit("", max_scrollback_entries),
            client,
            requested_capabilities,
            granted_capabilities: Vec::new(),
            attached: false,
        }
    }

    pub fn state(&self) -> &ViewState {
        &self.state
    }

    pub fn granted_capabilities(&self) -> &[ClientCapability] {
        &self.granted_capabilities
    }

    pub fn is_attached(&self) -> bool {
        self.attached
    }

    pub fn peer_label(&self) -> &str {
        self.transport.peer_label()
    }

    pub fn replace_transport(&mut self, transport: T) {
        self.transport = transport;
        self.clear_attachment();
    }

    /// A physical reconnect is a new daemon attachment even though it resumes
    /// the same logical session/watermark. A fresh id prevents two half-dead
    /// transports from repeatedly replacing one another.
    pub fn rotate_client_identity(&mut self) {
        self.client.id = format!("reconnect-{}", uuid::Uuid::new_v4());
    }

    pub async fn attach(&mut self, session_id: Option<String>) -> Result<(), SessionClientError> {
        self.clear_attachment();
        let result = self.attach_inner(session_id).await;
        if result.is_err() {
            self.clear_attachment();
        }
        result
    }

    async fn attach_inner(&mut self, session_id: Option<String>) -> Result<(), SessionClientError> {
        let requested_session_id = session_id.clone();
        let can_resume = session_id
            .as_deref()
            .is_some_and(|session_id| session_id == self.state.session_id());
        let attach_message = if can_resume {
            ClientMessage::Resume {
                protocol_version: PROTOCOL_VERSION,
                session_id: session_id.clone().expect("resume session id"),
                last_seen_seq: self.state.last_seq(),
                ticket: None,
                client: self.client.clone(),
                requested_capabilities: self.requested_capabilities.clone(),
            }
        } else {
            ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                session_id,
                ticket: None,
                client: self.client.clone(),
                requested_capabilities: self.requested_capabilities.clone(),
            }
        };
        self.transport.send(attach_message).await?;

        let mut attach_session_id = None;
        loop {
            let message = self.recv_message().await?;
            match message {
                ServerMessage::AttachOk {
                    protocol_version,
                    session_id,
                    granted_capabilities,
                    seq,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        return Err(SessionClientError::Protocol(format!(
                            "server protocol {protocol_version} does not match {PROTOCOL_VERSION}"
                        )));
                    }
                    if requested_session_id
                        .as_deref()
                        .is_some_and(|requested| requested != session_id)
                    {
                        return Err(SessionClientError::Protocol(
                            "AttachOk session does not match requested session".to_string(),
                        ));
                    }
                    self.granted_capabilities = granted_capabilities;
                    self.attached = true;
                    if !has_capability(&self.granted_capabilities, ClientCapability::Observe) {
                        self.state.apply_attach_watermark(session_id, seq);
                        return Ok(());
                    }
                    if can_resume
                        && self.state.session_id() == session_id
                        && self.state.last_seq() == seq
                    {
                        return Ok(());
                    }
                    attach_session_id = Some((session_id, seq));
                }
                ServerMessage::Event { seq, event } => {
                    let (_, expected_seq) = attach_session_id.as_ref().ok_or_else(|| {
                        SessionClientError::Protocol(
                            "replay event arrived before AttachOk".to_string(),
                        )
                    })?;
                    match self.state.apply_event(seq, event) {
                        ApplyOutcome::Applied | ApplyOutcome::Duplicate => {}
                        ApplyOutcome::Gap { .. } => {
                            return Err(SessionClientError::Protocol(
                                "reconnect replay contains a sequence gap".to_string(),
                            ));
                        }
                    }
                    if self.state.last_seq() == *expected_seq {
                        return Ok(());
                    }
                }
                ServerMessage::Snapshot { seq, state } => {
                    let (expected_session, expected_seq) =
                        attach_session_id.as_ref().ok_or_else(|| {
                            SessionClientError::Protocol(
                                "snapshot arrived before AttachOk".to_string(),
                            )
                        })?;
                    if state.session_id != *expected_session {
                        return Err(SessionClientError::Protocol(
                            "snapshot session does not match AttachOk".to_string(),
                        ));
                    }
                    if seq != state.seq || seq != *expected_seq {
                        return Err(SessionClientError::Protocol(
                            "attach snapshot sequence does not match AttachOk".to_string(),
                        ));
                    }
                    validate_snapshot_ids(&state)?;
                    self.state.apply_snapshot(state);
                    return Ok(());
                }
                ServerMessage::AttachDenied { code, reason } => {
                    return Err(SessionClientError::AttachDenied { code, reason });
                }
                ServerMessage::Error {
                    request_id,
                    code,
                    message,
                } => {
                    return Err(SessionClientError::Server {
                        request_id,
                        code,
                        message,
                    });
                }
                other => {
                    return Err(SessionClientError::Protocol(format!(
                        "unexpected attach response: {other:?}"
                    )));
                }
            }
        }
    }

    pub async fn receive(&mut self) -> Result<SessionClientOutcome, SessionClientError> {
        match self.recv_message().await? {
            ServerMessage::Event { seq, event } => match self.state.apply_event(seq, event) {
                ApplyOutcome::Applied => Ok(SessionClientOutcome::AppliedEvent(seq)),
                ApplyOutcome::Duplicate => Ok(SessionClientOutcome::IgnoredDuplicate(seq)),
                ApplyOutcome::Gap { expected_seq } => {
                    self.transport
                        .send(ClientMessage::SyncRequest {
                            last_seen_seq: self.state.last_seq(),
                        })
                        .await?;
                    Ok(SessionClientOutcome::RequestedSync { expected_seq })
                }
            },
            ServerMessage::Snapshot { seq, state } => {
                if seq != state.seq {
                    self.clear_attachment();
                    return Err(SessionClientError::Protocol(
                        "snapshot envelope sequence does not match state".to_string(),
                    ));
                }
                if state.session_id != self.state.session_id() {
                    self.clear_attachment();
                    return Err(SessionClientError::Protocol(
                        "snapshot session does not match attached session".to_string(),
                    ));
                }
                // Obsolete snapshots cannot affect the projection, so ignore
                // them before structural validation. Equal-sequence snapshots
                // remain authoritative rebases that can repair local drift.
                if seq < self.state.last_seq() {
                    return Ok(SessionClientOutcome::IgnoredDuplicate(seq));
                }
                validate_snapshot_ids(&state)?;
                self.state.apply_snapshot(state);
                Ok(SessionClientOutcome::AppliedSnapshot(seq))
            }
            ServerMessage::Error {
                request_id,
                code,
                message,
            } => Ok(SessionClientOutcome::ServerError {
                request_id,
                code,
                message,
            }),
            ServerMessage::Pong => Ok(SessionClientOutcome::Pong),
            ServerMessage::CommandResult {
                request_id,
                operation,
                changed,
            } => Ok(SessionClientOutcome::CommandResult {
                request_id,
                operation,
                changed,
            }),
            ServerMessage::SessionList {
                request_id,
                workspace,
                sessions,
                next_cursor,
                incomplete,
            } => Ok(SessionClientOutcome::SessionList {
                request_id,
                workspace,
                sessions,
                next_cursor,
                incomplete,
            }),
            ServerMessage::Usage { request_id, usage } => {
                Ok(SessionClientOutcome::Usage { request_id, usage })
            }
            ServerMessage::Revoked { code, reason } => {
                self.attached = false;
                Ok(SessionClientOutcome::Revoked { code, reason })
            }
            ServerMessage::AttachOk { .. } | ServerMessage::AttachDenied { .. } => {
                Err(SessionClientError::Protocol(
                    "attach response outside attach handshake".to_string(),
                ))
            }
        }
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Prompt)?;
        let request_id = self.next_request_id("prompt");
        self.transport
            .send(ClientMessage::Prompt {
                request_id: request_id.clone(),
                text: text.into(),
            })
            .await?;
        Ok(request_id)
    }

    pub async fn interrupt(&mut self) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Interrupt)?;
        let request_id = self.next_request_id("interrupt");
        self.transport
            .send(ClientMessage::Interrupt {
                request_id: Some(request_id.clone()),
            })
            .await?;
        Ok(request_id)
    }

    pub async fn approve(
        &mut self,
        approval_id: impl Into<String>,
        decision: ApprovalDecision,
    ) -> Result<(), SessionClientError> {
        let required = if decision == ApprovalDecision::AllowAlways {
            ClientCapability::ApproveAlways
        } else {
            ClientCapability::ApproveOnce
        };
        self.require(required)?;
        self.transport
            .send(ClientMessage::ApprovalResponse {
                approval_id: approval_id.into(),
                decision,
            })
            .await?;
        Ok(())
    }

    pub async fn set_config(
        &mut self,
        config_id: impl Into<String>,
        value: RuntimeValue,
    ) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Configure)?;
        let request_id = self.next_request_id("set-config");
        self.transport
            .send(ClientMessage::SetConfig {
                request_id: Some(request_id.clone()),
                config_id: config_id.into(),
                value,
            })
            .await?;
        Ok(request_id)
    }

    pub async fn stop_session(&mut self) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Stop)?;
        let request_id = self.next_request_id("stop");
        self.transport
            .send(ClientMessage::StopSession {
                request_id: request_id.clone(),
            })
            .await?;
        Ok(request_id)
    }

    pub async fn clear_history(&mut self) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Configure)?;
        let request_id = self.next_request_id("clear");
        self.transport
            .send(ClientMessage::ClearHistory {
                request_id: request_id.clone(),
            })
            .await?;
        Ok(request_id)
    }

    pub async fn get_usage(&mut self) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Observe)?;
        let request_id = self.next_request_id("usage");
        self.transport
            .send(ClientMessage::GetUsage {
                request_id: request_id.clone(),
            })
            .await?;
        Ok(request_id)
    }

    pub async fn list_sessions(
        &mut self,
        cursor: Option<String>,
    ) -> Result<String, SessionClientError> {
        self.require(ClientCapability::Observe)?;
        let request_id = self.next_request_id("list");
        self.transport
            .send(ClientMessage::ListSessions {
                request_id: request_id.clone(),
                cursor,
            })
            .await?;
        Ok(request_id)
    }

    pub async fn sync(&mut self) -> Result<(), SessionClientError> {
        self.require(ClientCapability::Observe)?;
        self.transport
            .send(ClientMessage::SyncRequest {
                last_seen_seq: self.state.last_seq(),
            })
            .await?;
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<(), SessionClientError> {
        self.transport.send(ClientMessage::Ping).await?;
        Ok(())
    }

    pub async fn detach(&mut self) -> Result<(), SessionClientError> {
        self.transport.send(ClientMessage::Detach).await?;
        self.attached = false;
        Ok(())
    }

    fn require(&self, capability: ClientCapability) -> Result<(), SessionClientError> {
        if !self.attached {
            return Err(SessionClientError::Disconnected);
        }
        if has_capability(&self.granted_capabilities, capability) {
            Ok(())
        } else {
            Err(SessionClientError::MissingCapability(capability))
        }
    }

    fn next_request_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    async fn recv_message(&mut self) -> Result<ServerMessage, SessionClientError> {
        match self.transport.recv().await {
            Ok(Some(message)) => Ok(message),
            Ok(None) => {
                self.clear_attachment();
                Err(SessionClientError::Disconnected)
            }
            Err(error) => {
                self.clear_attachment();
                Err(SessionClientError::Transport(error))
            }
        }
    }

    fn clear_attachment(&mut self) {
        self.attached = false;
        self.granted_capabilities.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_transport::{in_memory_transport_pair, ClientTransport};
    use crate::session_protocol::{
        ActiveToolState, ClientKind, ContextUsage, HistoryWindow, SessionEvent, SessionSnapshot,
        ToolCallStateStatus, TurnStatus,
    };

    fn client() -> ClientInfo {
        ClientInfo {
            id: "session-client-1".to_string(),
            kind: ClientKind::Headless,
            label: "session client test".to_string(),
        }
    }

    fn snapshot(session_id: &str, seq: u64) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.to_string(),
            seq,
            turn_status: TurnStatus::Idle,
            durability_status: crate::session_protocol::DurabilityStatus::Saved,
            timeline: Vec::new(),
            active_tools: Vec::new(),
            history_window: HistoryWindow::complete(0),
            pending_approvals: Vec::new(),
            runtime_options: Vec::new(),
            context_usage: Some(ContextUsage::new(1, Some(100), 0, None)),
        }
    }

    #[test]
    fn frame_limit_mismatch_is_classified_independently_of_operation() {
        let error = SessionClientError::Transport(TransportError::FrameTooLarge { max_bytes: 512 });
        assert_eq!(error.frame_limit_mismatch(), Some(512));
        assert!(error.to_string().starts_with("frame_limit_mismatch:"));
        assert!(error.to_string().contains("max_frame_bytes=512"));
    }

    #[test]
    fn snapshot_accepts_distinct_active_occurrences_with_reused_provider_id() {
        let mut state = snapshot("session-1", 0);
        state.active_tools = [1, 2]
            .into_iter()
            .map(|occurrence_id| ActiveToolState {
                occurrence_id,
                tool_call_id: "provider-reused".to_string(),
                name: "exec".to_string(),
                title: format!("occurrence {occurrence_id}"),
                status: ToolCallStateStatus::InProgress,
                content_truncated: false,
            })
            .collect();

        validate_snapshot_ids(&state).unwrap();
    }

    #[tokio::test]
    async fn attach_applies_snapshot_and_capabilities() {
        let (server, client_transport) = in_memory_transport_pair(4, "daemon");
        let server_task = tokio::spawn(async move {
            assert!(matches!(
                server.recv().await.unwrap(),
                Some(ClientMessage::Attach { .. })
            ));
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe, ClientCapability::Prompt],
            100,
        );

        frontend.attach(None).await.unwrap();

        assert!(frontend.is_attached());
        assert_eq!(frontend.state().session_id(), "session-1");
        assert!(has_capability(
            frontend.granted_capabilities(),
            ClientCapability::Prompt
        ));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_attach_rejects_unrequested_session_identity() {
        let (server, client_transport) = in_memory_transport_pair(4, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "wrong-session".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        let error = frontend
            .attach(Some("requested-session".to_string()))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("AttachOk session does not match requested session"));
        assert!(!frontend.is_attached());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn attach_rejects_duplicate_canonical_snapshot_ids() {
        let (server, client_transport) = in_memory_transport_pair(4, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            let mut invalid = snapshot("session-1", 0);
            invalid.timeline = ["first", "second"]
                .into_iter()
                .map(|text| crate::session_protocol::TimelineEntry {
                    id: 1,
                    order: 1,
                    entry: crate::session_protocol::TimelineEntryKind::User {
                        text: text.to_string(),
                        content_truncated: false,
                    },
                })
                .collect();
            invalid.history_window = HistoryWindow::complete(2);
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: invalid,
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        let error = frontend.attach(None).await.unwrap_err();
        assert!(error.to_string().contains("duplicate timeline id"));
        assert!(!frontend.is_attached());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn event_gap_requests_snapshot_sync_without_applying_event() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Event {
                    seq: 2,
                    event: SessionEvent::UserMessage {
                        text: "gap".to_string(),
                        request_id: None,
                    },
                })
                .await
                .unwrap();
            assert_eq!(
                server.recv().await.unwrap(),
                Some(ClientMessage::SyncRequest { last_seen_seq: 0 })
            );
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend.attach(None).await.unwrap();

        assert_eq!(
            frontend.receive().await.unwrap(),
            SessionClientOutcome::RequestedSync { expected_seq: 1 }
        );
        assert_eq!(frontend.state().last_seq(), 0);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_resumes_from_last_seen_sequence_without_snapshot() {
        let (first_server, first_transport) = in_memory_transport_pair(8, "daemon-1");
        let first_task = tokio::spawn(async move {
            let _ = first_server.recv().await.unwrap();
            first_server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 1,
                })
                .await
                .unwrap();
            first_server
                .send(&ServerMessage::Snapshot {
                    seq: 1,
                    state: snapshot("session-1", 1),
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            first_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();
        first_task.await.unwrap();

        let (second_server, second_transport) = in_memory_transport_pair(8, "daemon-2");
        let second_task = tokio::spawn(async move {
            assert!(matches!(
                second_server.recv().await.unwrap(),
                Some(ClientMessage::Resume {
                    session_id,
                    last_seen_seq: 1,
                    ..
                }) if session_id == "session-1"
            ));
            second_server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 3,
                })
                .await
                .unwrap();
            second_server
                .send(&ServerMessage::Event {
                    seq: 2,
                    event: SessionEvent::AssistantDelta {
                        text: "resumed".to_string(),
                    },
                })
                .await
                .unwrap();
            second_server
                .send(&ServerMessage::Event {
                    seq: 3,
                    event: SessionEvent::AssistantDone {
                        outcome: crate::session_protocol::AssistantOutcome::Completed,
                    },
                })
                .await
                .unwrap();
        });
        frontend.replace_transport(second_transport);

        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();

        assert_eq!(frontend.state().last_seq(), 3);
        assert_eq!(frontend.state().transcript()[0].text, "resumed");
        second_task.await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_rejects_attach_ok_for_different_session() {
        let (first_server, first_transport) = in_memory_transport_pair(4, "daemon-1");
        let first_task = tokio::spawn(async move {
            let _ = first_server.recv().await.unwrap();
            first_server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            first_server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            first_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();
        first_task.await.unwrap();

        let (second_server, second_transport) = in_memory_transport_pair(4, "daemon-2");
        let second_task = tokio::spawn(async move {
            assert!(matches!(
                second_server.recv().await.unwrap(),
                Some(ClientMessage::Resume { .. })
            ));
            second_server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-2".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
        });
        frontend.replace_transport(second_transport);

        assert!(matches!(
            frontend.attach(Some("session-1".to_string())).await,
            Err(SessionClientError::Protocol(_))
        ));
        assert_eq!(frontend.state().session_id(), "session-1");
        assert!(!frontend.is_attached());
        second_task.await.unwrap();
    }

    #[tokio::test]
    async fn commands_use_granted_capabilities_and_correlated_ids() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![
                        ClientCapability::Prompt,
                        ClientCapability::ApproveOnce,
                    ],
                    seq: 0,
                })
                .await
                .unwrap();
            assert!(matches!(
                server.recv().await.unwrap(),
                Some(ClientMessage::Prompt { request_id, text })
                    if request_id.starts_with("prompt-") && text == "run"
            ));
            assert_eq!(
                server.recv().await.unwrap(),
                Some(ClientMessage::ApprovalResponse {
                    approval_id: "approval-1".to_string(),
                    decision: ApprovalDecision::Deny,
                })
            );
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![
                ClientCapability::Prompt,
                ClientCapability::Interrupt,
                ClientCapability::ApproveOnce,
            ],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();

        assert!(frontend.prompt("run").await.unwrap().starts_with("prompt-"));
        frontend
            .approve("approval-1", ApprovalDecision::Deny)
            .await
            .unwrap();
        assert!(matches!(
            frontend.interrupt().await,
            Err(SessionClientError::MissingCapability(
                ClientCapability::Interrupt
            ))
        ));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_clears_attachment_and_capabilities() {
        let (server, client_transport) = in_memory_transport_pair(4, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Prompt],
                    seq: 7,
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Prompt],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();
        assert_eq!(frontend.state().session_id(), "session-1");
        assert_eq!(frontend.state().last_seq(), 7);
        server_task.await.unwrap();

        assert!(matches!(
            frontend.receive().await,
            Err(SessionClientError::Disconnected)
        ));
        assert!(!frontend.is_attached());
        assert!(frontend.granted_capabilities().is_empty());
    }

    #[tokio::test]
    async fn failed_attach_after_attach_ok_rolls_back_local_state() {
        let (server, client_transport) = in_memory_transport_pair(4, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Error {
                    request_id: None,
                    code: "snapshot_failed".to_string(),
                    message: "snapshot unavailable".to_string(),
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe, ClientCapability::Prompt],
            100,
        );

        assert!(matches!(
            frontend.attach(Some("session-1".to_string())).await,
            Err(SessionClientError::Server { .. })
        ));
        assert!(!frontend.is_attached());
        assert!(frontend.granted_capabilities().is_empty());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn midstream_snapshot_cannot_change_attached_session() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 1,
                    state: snapshot("session-2", 1),
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();

        assert!(matches!(
            frontend.receive().await,
            Err(SessionClientError::Protocol(_))
        ));
        assert_eq!(frontend.state().session_id(), "session-1");
        assert!(!frontend.is_attached());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn stale_midstream_snapshot_cannot_rewind_sequence() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Event {
                    seq: 1,
                    event: SessionEvent::UserMessage {
                        text: "new".to_string(),
                        request_id: None,
                    },
                })
                .await
                .unwrap();
            let mut stale = snapshot("session-1", 0);
            stale.timeline = ["obsolete-a", "obsolete-b"]
                .into_iter()
                .enumerate()
                .map(|(index, text)| crate::session_protocol::TimelineEntry {
                    id: 1,
                    order: index as u64 + 1,
                    entry: crate::session_protocol::TimelineEntryKind::User {
                        text: text.to_string(),
                        content_truncated: false,
                    },
                })
                .collect();
            stale.history_window = HistoryWindow::complete(2);
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: stale,
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();
        assert_eq!(
            frontend.receive().await.unwrap(),
            SessionClientOutcome::AppliedEvent(1)
        );
        assert_eq!(
            frontend.receive().await.unwrap(),
            SessionClientOutcome::IgnoredDuplicate(0)
        );
        assert_eq!(frontend.state().last_seq(), 1);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn equal_sequence_snapshot_canonically_rebases_view() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Event {
                    seq: 1,
                    event: SessionEvent::UserMessage {
                        text: "provisional".to_string(),
                        request_id: None,
                    },
                })
                .await
                .unwrap();
            let mut canonical = snapshot("session-1", 1);
            canonical
                .timeline
                .push(crate::session_protocol::TimelineEntry {
                    id: 7,
                    order: 7,
                    entry: crate::session_protocol::TimelineEntryKind::Assistant {
                        text: "canonical".to_string(),
                        content_truncated: false,
                    },
                });
            canonical.history_window = HistoryWindow::complete(1);
            server
                .send(&ServerMessage::Snapshot {
                    seq: 1,
                    state: canonical,
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();
        assert_eq!(
            frontend.receive().await.unwrap(),
            SessionClientOutcome::AppliedEvent(1)
        );
        assert_eq!(
            frontend.receive().await.unwrap(),
            SessionClientOutcome::AppliedSnapshot(1)
        );
        assert_eq!(frontend.state().transcript().len(), 1);
        assert_eq!(frontend.state().transcript()[0].text, "canonical");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn midstream_snapshot_rejects_envelope_state_sequence_mismatch() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            let _ = server.recv().await.unwrap();
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "session-1".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("session-1", 0),
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 2,
                    state: snapshot("session-1", 1),
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend
            .attach(Some("session-1".to_string()))
            .await
            .unwrap();
        let error = frontend.receive().await.unwrap_err();
        assert!(error
            .to_string()
            .contains("snapshot envelope sequence does not match state"));
        assert!(!frontend.is_attached());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn session_list_preserves_typed_rows_and_workspace() {
        let (server, client_transport) = in_memory_transport_pair(8, "daemon");
        let server_task = tokio::spawn(async move {
            assert!(matches!(
                server.recv().await.unwrap(),
                Some(ClientMessage::Attach { .. })
            ));
            server
                .send(&ServerMessage::AttachOk {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: "current".to_string(),
                    granted_capabilities: vec![ClientCapability::Observe],
                    seq: 0,
                })
                .await
                .unwrap();
            server
                .send(&ServerMessage::Snapshot {
                    seq: 0,
                    state: snapshot("current", 0),
                })
                .await
                .unwrap();
            let Some(ClientMessage::ListSessions { request_id, .. }) = server.recv().await.unwrap()
            else {
                panic!("expected list sessions");
            };
            server
                .send(&ServerMessage::SessionList {
                    request_id,
                    workspace: Some(SessionWorkspace {
                        id: "ws_1".to_string(),
                        label: "workspace".to_string(),
                    }),
                    sessions: vec![SessionListEntry {
                        session_id: "saved".to_string(),
                        active: false,
                        attached_clients: 0,
                        model: Some("model".to_string()),
                        updated_at_unix_ms: Some(42),
                        preview: Some("hello".to_string()),
                        message_count: Some(2),
                        turn_status: None,
                    }],
                    next_cursor: Some("v1_cursor".to_string()),
                    incomplete: true,
                })
                .await
                .unwrap();
        });
        let mut frontend = SessionClient::new(
            client_transport,
            client(),
            vec![ClientCapability::Observe],
            100,
        );
        frontend.attach(None).await.unwrap();
        let request_id = frontend.list_sessions(None).await.unwrap();
        let outcome = frontend.receive().await.unwrap();
        let SessionClientOutcome::SessionList {
            request_id: response_id,
            workspace,
            sessions,
            next_cursor,
            incomplete,
        } = outcome
        else {
            panic!("expected typed session list");
        };
        assert_eq!(response_id, request_id);
        assert_eq!(workspace.unwrap().id, "ws_1");
        assert_eq!(sessions[0].preview.as_deref(), Some("hello"));
        assert_eq!(next_cursor.as_deref(), Some("v1_cursor"));
        assert!(incomplete);
        server_task.await.unwrap();
    }
}
