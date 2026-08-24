//! Daemon-owned interactive agent sessions (Vikunja #1096).

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::client_transport::{ClientTransport, TransportError, UnixSocketTransport};
use crate::session_core::{
    RuntimeConfigError, SessionCore, SessionEventSubscription, SessionReplay,
};
use crate::session_protocol::{
    AttachDeniedCode, ClientCapability, ClientInfo, ClientMessage, ProtocolLimits, ServerMessage,
    SessionEvent, SessionListEntry, SessionSnapshot, ToolCallState, ToolCallStateStatus,
    TranscriptEntry, TranscriptRole,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOpenMode {
    Create,
    Load,
}

#[async_trait::async_trait]
pub trait SessionFactory: Send + Sync {
    async fn open(
        &self,
        session_id: &str,
        mode: SessionOpenMode,
    ) -> Result<Arc<SessionCore>, String>;

    fn persisted_session_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDaemonError {
    DuplicateSession(String),
    SessionLimitReached { max: usize },
    SessionNotFound(String),
    SessionStopped(String),
    DuplicateClient(String),
    ClientLimitReached { max: usize },
    EventSubscription(String),
    FactoryUnavailable,
    OpenFailed(String),
    ShuttingDown,
}

#[derive(Clone)]
pub struct CapabilityPolicy {
    allowed: std::collections::HashSet<ClientCapability>,
}

impl CapabilityPolicy {
    pub fn local_trusted() -> Self {
        Self {
            allowed: [
                ClientCapability::Observe,
                ClientCapability::Prompt,
                ClientCapability::Configure,
                ClientCapability::Interrupt,
                ClientCapability::Stop,
                ClientCapability::ApproveOnce,
                ClientCapability::ApproveAlways,
            ]
            .into_iter()
            .collect(),
        }
    }

    #[cfg(test)]
    fn allowing(capabilities: impl IntoIterator<Item = ClientCapability>) -> Self {
        Self {
            allowed: capabilities.into_iter().collect(),
        }
    }

    fn grant(&self, requested: Vec<ClientCapability>) -> Vec<ClientCapability> {
        let mut granted = Vec::new();
        for capability in requested {
            if self.allowed.contains(&capability) && !granted.contains(&capability) {
                granted.push(capability);
            }
        }
        granted
    }
}

struct ClientAttachment {
    _info: ClientInfo,
    capabilities: Vec<ClientCapability>,
    attachment_id: u64,
    replacement_notifier: tokio::sync::watch::Sender<bool>,
}

struct SessionEntry {
    core: Arc<SessionCore>,
    admission: Arc<tokio::sync::Mutex<()>>,
    clients: Mutex<HashMap<String, ClientAttachment>>,
    max_clients: usize,
    stopped: AtomicBool,
    stop_notifier: tokio::sync::watch::Sender<bool>,
    last_detached_at: Mutex<Option<tokio::time::Instant>>,
    snapshot: Arc<Mutex<SnapshotState>>,
    _snapshot_subscription: SessionEventSubscription,
    active_prompt_tasks: AtomicUsize,
    prompt_tasks_changed: tokio::sync::Notify,
}

pub struct SessionDaemon {
    sessions: Mutex<HashMap<String, Arc<SessionEntry>>>,
    max_sessions: usize,
    max_clients_per_session: usize,
    event_queue_capacity: usize,
    max_snapshot_entries: usize,
    idle_retention: Option<std::time::Duration>,
    session_list_page_size: usize,
    factory: Option<Arc<dyn SessionFactory>>,
    creation_gate: tokio::sync::Mutex<()>,
    creating_sessions: Mutex<std::collections::HashSet<String>>,
    shutting_down: AtomicBool,
    shutdown_grace: std::time::Duration,
    active_client_tasks: AtomicUsize,
    client_tasks_changed: tokio::sync::Notify,
    next_attachment_id: AtomicU64,
}

impl SessionDaemon {
    #[cfg(test)]
    pub fn new(
        max_sessions: usize,
        max_clients_per_session: usize,
        event_queue_capacity: usize,
        max_snapshot_entries: usize,
    ) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            max_sessions,
            max_clients_per_session,
            event_queue_capacity: event_queue_capacity.max(1),
            max_snapshot_entries: max_snapshot_entries.max(1),
            idle_retention: None,
            session_list_page_size: max_sessions.max(1),
            factory: None,
            creation_gate: tokio::sync::Mutex::new(()),
            creating_sessions: Mutex::new(std::collections::HashSet::new()),
            shutting_down: AtomicBool::new(false),
            shutdown_grace: std::time::Duration::from_secs(5),
            active_client_tasks: AtomicUsize::new(0),
            client_tasks_changed: tokio::sync::Notify::new(),
            next_attachment_id: AtomicU64::new(1),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_factory(
        max_sessions: usize,
        max_clients_per_session: usize,
        event_queue_capacity: usize,
        max_snapshot_entries: usize,
        idle_retention: Option<std::time::Duration>,
        session_list_page_size: usize,
        shutdown_grace: std::time::Duration,
        factory: Arc<dyn SessionFactory>,
    ) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            max_sessions,
            max_clients_per_session,
            event_queue_capacity: event_queue_capacity.max(1),
            max_snapshot_entries: max_snapshot_entries.max(1),
            idle_retention,
            session_list_page_size: session_list_page_size.max(1),
            factory: Some(factory),
            creation_gate: tokio::sync::Mutex::new(()),
            creating_sessions: Mutex::new(std::collections::HashSet::new()),
            shutting_down: AtomicBool::new(false),
            shutdown_grace,
            active_client_tasks: AtomicUsize::new(0),
            client_tasks_changed: tokio::sync::Notify::new(),
            next_attachment_id: AtomicU64::new(1),
        }
    }

    async fn open_session(
        &self,
        requested_session_id: Option<String>,
    ) -> Result<(String, bool), SessionDaemonError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SessionDaemonError::ShuttingDown);
        }
        if let Some(session_id) = requested_session_id.as_deref() {
            if self.session(session_id).is_some() {
                return Ok((session_id.to_string(), false));
            }
        }
        let factory = Arc::clone(
            self.factory
                .as_ref()
                .ok_or(SessionDaemonError::FactoryUnavailable)?,
        );
        let (session_id, mode, generated) = match requested_session_id {
            Some(session_id) => (session_id, SessionOpenMode::Load, false),
            None => (
                uuid::Uuid::new_v4().to_string(),
                SessionOpenMode::Create,
                true,
            ),
        };
        {
            let _creation = self.creation_gate.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(SessionDaemonError::ShuttingDown);
            }
            if self.session(&session_id).is_some() {
                return Ok((session_id, false));
            }
            let sessions_len = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len();
            let mut creating = self
                .creating_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sessions_len.saturating_add(creating.len()) >= self.max_sessions {
                return Err(SessionDaemonError::SessionLimitReached {
                    max: self.max_sessions,
                });
            }
            if !creating.insert(session_id.clone()) {
                return Err(SessionDaemonError::DuplicateSession(session_id));
            }
        }
        let opened = match factory.open(&session_id, mode).await {
            Ok(core) => {
                let snapshot = core
                    .initial_snapshot(session_id.clone(), self.max_snapshot_entries)
                    .await;
                Ok((core, snapshot))
            }
            Err(error) => Err(SessionDaemonError::OpenFailed(error)),
        };
        let _creation = self.creation_gate.lock().await;
        self.creating_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session_id);
        let (core, snapshot) = opened?;
        if self.shutting_down.load(Ordering::Acquire) {
            if generated {
                let _ = core.delete_persisted();
            }
            return Err(SessionDaemonError::ShuttingDown);
        }
        self.create_session_with_snapshot(session_id.clone(), core, snapshot)?;
        Ok((session_id, generated))
    }

    #[cfg(test)]
    pub fn create_session(
        &self,
        session_id: String,
        core: Arc<SessionCore>,
    ) -> Result<(), SessionDaemonError> {
        let mut snapshot =
            SnapshotState::new(session_id.clone(), self.max_snapshot_entries).snapshot;
        snapshot.runtime_options = core.runtime_options();
        self.create_session_with_snapshot(session_id, core, snapshot)
    }

    fn create_session_with_snapshot(
        &self,
        session_id: String,
        core: Arc<SessionCore>,
        initial_snapshot: SessionSnapshot,
    ) -> Result<(), SessionDaemonError> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if sessions.contains_key(&session_id) {
            return Err(SessionDaemonError::DuplicateSession(session_id));
        }
        if sessions.len() >= self.max_sessions {
            return Err(SessionDaemonError::SessionLimitReached {
                max: self.max_sessions,
            });
        }
        let snapshot = Arc::new(Mutex::new(SnapshotState::from_snapshot(
            initial_snapshot,
            self.max_snapshot_entries,
        )));
        let snapshot_for_events = Arc::clone(&snapshot);
        let snapshot_subscription = core
            .events
            .subscribe(
                self.max_clients_per_session.saturating_add(1),
                Arc::new(move |seq, event| {
                    snapshot_for_events
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .apply(seq, event);
                }),
            )
            .map_err(|error| SessionDaemonError::EventSubscription(format!("{error:?}")))?;
        let (stop_notifier, _) = tokio::sync::watch::channel(false);
        sessions.insert(
            session_id,
            Arc::new(SessionEntry {
                core,
                admission: Arc::new(tokio::sync::Mutex::new(())),
                clients: Mutex::new(HashMap::new()),
                max_clients: self.max_clients_per_session,
                stopped: AtomicBool::new(false),
                stop_notifier,
                last_detached_at: Mutex::new(None),
                snapshot,
                _snapshot_subscription: snapshot_subscription,
                active_prompt_tasks: AtomicUsize::new(0),
                prompt_tasks_changed: tokio::sync::Notify::new(),
            }),
        );
        Ok(())
    }

    #[cfg(test)]
    pub async fn attach(
        &self,
        session_id: &str,
        client: ClientInfo,
        capabilities: Vec<ClientCapability>,
    ) -> Result<AttachedSession, SessionDaemonError> {
        self.attach_internal(session_id, client, capabilities, false)
            .await
    }

    async fn attach_internal(
        &self,
        session_id: &str,
        client: ClientInfo,
        capabilities: Vec<ClientCapability>,
        replace_existing: bool,
    ) -> Result<AttachedSession, SessionDaemonError> {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionDaemonError::SessionNotFound(session_id.to_string()))?;
        let handshake_admission = Arc::clone(&entry.admission).lock_owned().await;
        let attachment_id;
        let replacement_receiver;
        {
            let mut clients = entry
                .clients
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if entry.stopped.load(Ordering::Acquire) {
                return Err(SessionDaemonError::SessionStopped(session_id.to_string()));
            }
            if clients.contains_key(&client.id) && !replace_existing {
                return Err(SessionDaemonError::DuplicateClient(client.id));
            }
            if !clients.contains_key(&client.id) && clients.len() >= entry.max_clients {
                return Err(SessionDaemonError::ClientLimitReached {
                    max: entry.max_clients,
                });
            }
            if replace_existing {
                if let Some(previous) = clients.remove(&client.id) {
                    previous.replacement_notifier.send_replace(true);
                }
            }
            attachment_id = self.next_attachment_id.fetch_add(1, Ordering::AcqRel);
            let (replacement_notifier, receiver) = tokio::sync::watch::channel(false);
            replacement_receiver = receiver;
            clients.insert(
                client.id.clone(),
                ClientAttachment {
                    _info: client.clone(),
                    capabilities: capabilities.clone(),
                    attachment_id,
                    replacement_notifier,
                },
            );
            let approve_once = clients
                .values()
                .filter(|client| client.capabilities.contains(&ClientCapability::ApproveOnce))
                .count();
            let approve_always = clients
                .values()
                .filter(|client| {
                    client
                        .capabilities
                        .contains(&ClientCapability::ApproveAlways)
                })
                .count();
            entry
                .core
                .approvals
                .set_eligible_client_counts(approve_once, approve_always);
            *entry
                .last_detached_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
        Ok(AttachedSession {
            entry,
            client_id: client.id,
            capabilities,
            attachment_id,
            replacement_receiver,
            handshake_admission: Some(handshake_admission),
        })
    }

    pub fn session(&self, session_id: &str) -> Option<Arc<SessionCore>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|entry| Arc::clone(&entry.core))
    }

    fn list_sessions(&self, cursor: Option<&str>) -> (Vec<SessionListEntry>, Option<String>) {
        let mut sessions = std::collections::BTreeMap::new();
        {
            let active = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (session_id, entry) in active.iter() {
                let attached_clients = entry
                    .clients
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len();
                sessions.insert(
                    session_id.clone(),
                    SessionListEntry {
                        session_id: session_id.clone(),
                        active: true,
                        attached_clients,
                    },
                );
            }
        }
        if let Some(factory) = self.factory.as_ref() {
            for session_id in factory.persisted_session_ids() {
                sessions
                    .entry(session_id.clone())
                    .or_insert(SessionListEntry {
                        session_id,
                        active: false,
                        attached_clients: 0,
                    });
            }
        }
        let mut page: Vec<SessionListEntry> = sessions
            .into_values()
            .filter(|entry| cursor.is_none_or(|cursor| entry.session_id.as_str() > cursor))
            .take(self.session_list_page_size.saturating_add(1))
            .collect();
        let next_cursor = (page.len() > self.session_list_page_size)
            .then(|| page[self.session_list_page_size - 1].session_id.clone());
        page.truncate(self.session_list_page_size);
        (page, next_cursor)
    }

    async fn evict_idle_sessions(&self) {
        let Some(retention) = self.idle_retention else {
            return;
        };
        let now = tokio::time::Instant::now();
        let session_ids: Vec<String> = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter_map(|(session_id, entry)| {
                let idle = entry
                    .last_detached_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_some_and(|detached| now.duration_since(detached) >= retention);
                idle.then(|| session_id.clone())
            })
            .collect();
        if session_ids.is_empty() {
            return;
        }
        let _creation = self.creation_gate.lock().await;
        for session_id in session_ids {
            let entry = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&session_id)
                .cloned();
            let Some(entry) = entry else {
                continue;
            };
            let admission = entry.admission.lock().await;
            let still_idle = entry
                .clients
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
                && entry
                    .last_detached_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_some_and(|detached| now.duration_since(detached) >= retention)
                && entry.active_prompt_tasks.load(Ordering::Acquire) == 0
                && !entry.core.turn.is_active();
            if !still_idle {
                continue;
            }
            let removed = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if sessions
                    .get(&session_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    sessions.remove(&session_id)
                } else {
                    None
                }
            };
            let Some(entry) = removed else {
                continue;
            };
            entry.stopped.store(true, Ordering::Release);
            drop(admission);
            let _ = Self::finish_removed_entry(entry, "idle_retention", false);
        }
    }

    pub async fn stop_session(&self, session_id: &str) -> std::io::Result<bool> {
        let _creation = self.creation_gate.lock().await;
        self.end_session(session_id, "stopped", true).await
    }

    async fn end_session(
        &self,
        session_id: &str,
        reason: &str,
        delete_persisted: bool,
    ) -> std::io::Result<bool> {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned();
        let Some(entry) = entry else {
            return Ok(false);
        };
        let admission = entry.admission.lock().await;
        let removed = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sessions
                .get(session_id)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                sessions.remove(session_id)
            } else {
                None
            }
        };
        let Some(entry) = removed else {
            return Ok(false);
        };
        entry.stopped.store(true, Ordering::Release);
        let _ = entry.core.cancel_turn();
        drop(admission);
        let _ = tokio::time::timeout(self.shutdown_grace, async {
            loop {
                let changed = entry.prompt_tasks_changed.notified();
                if entry.active_prompt_tasks.load(Ordering::Acquire) == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await;
        Self::finish_removed_entry(entry, reason, delete_persisted)?;
        Ok(true)
    }

    fn finish_removed_entry(
        entry: Arc<SessionEntry>,
        reason: &str,
        delete_persisted: bool,
    ) -> std::io::Result<()> {
        entry
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        entry.core.cleanup_cancelled_turn();
        let delete_result = delete_persisted
            .then(|| entry.core.delete_persisted())
            .transpose();
        let _ = entry
            .core
            .events
            .emit(crate::session_protocol::SessionEvent::SessionEnding {
                reason: reason.to_string(),
            });
        entry.stop_notifier.send_replace(true);
        delete_result?;
        Ok(())
    }

    fn rollback_generated_session(&self, session_id: &str) {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
        if let Some(entry) = entry {
            entry.stopped.store(true, Ordering::Release);
            let _ = entry.core.cancel_turn();
            let _ = Self::finish_removed_entry(entry, "attach_failed", true);
        }
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let _creation = self.creation_gate.lock().await;
        let session_ids: Vec<String> = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        for session_id in session_ids {
            let _ = self
                .end_session(&session_id, "daemon_shutdown", false)
                .await;
        }
        let _ = tokio::time::timeout(self.shutdown_grace, async {
            loop {
                let changed = self.client_tasks_changed.notified();
                if self.active_client_tasks.load(Ordering::Acquire) == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await;
    }

    #[cfg(test)]
    pub fn attached_client_count(&self, session_id: &str) -> Option<usize> {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()?;
        let count = entry
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        Some(count)
    }

    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub async fn serve_client<T: ClientTransport>(
        &self,
        transport: T,
        limits: &ProtocolLimits,
    ) -> Result<(), TransportError> {
        self.serve_client_with_policy(transport, limits, &CapabilityPolicy::local_trusted())
            .await
    }

    pub async fn serve_client_with_capabilities<T: ClientTransport>(
        &self,
        transport: T,
        limits: &ProtocolLimits,
        allowed_capabilities: Vec<ClientCapability>,
    ) -> Result<(), TransportError> {
        let capability_policy = CapabilityPolicy {
            allowed: allowed_capabilities.into_iter().collect(),
        };
        self.serve_client_with_policy(transport, limits, &capability_policy)
            .await
    }

    async fn serve_client_with_policy<T: ClientTransport>(
        &self,
        transport: T,
        limits: &ProtocolLimits,
        capability_policy: &CapabilityPolicy,
    ) -> Result<(), TransportError> {
        let Some(first) = transport.recv().await? else {
            return Ok(());
        };
        if let Err(error) = limits.validate_client_message(&first) {
            transport
                .send(&ServerMessage::AttachDenied {
                    code: Some(AttachDeniedCode::InvalidMessage),
                    reason: format!("invalid attach message: {error:?}"),
                })
                .await?;
            return Ok(());
        }
        let (protocol_version, session_id, resume_seq, client, requested_capabilities) = match first
        {
            ClientMessage::Attach {
                protocol_version,
                session_id,
                client,
                requested_capabilities,
                ..
            } => (
                protocol_version,
                session_id,
                None,
                client,
                requested_capabilities,
            ),
            ClientMessage::Resume {
                protocol_version,
                session_id,
                last_seen_seq,
                client,
                requested_capabilities,
                ..
            } => (
                protocol_version,
                Some(session_id),
                Some(last_seen_seq),
                client,
                requested_capabilities,
            ),
            _ => {
                transport
                    .send(&ServerMessage::AttachDenied {
                        code: Some(AttachDeniedCode::InvalidMessage),
                        reason: "first message must be attach or resume".to_string(),
                    })
                    .await?;
                return Ok(());
            }
        };
        if protocol_version != crate::session_protocol::PROTOCOL_VERSION {
            transport
                .send(&ServerMessage::AttachDenied {
                    code: Some(AttachDeniedCode::ProtocolVersion),
                    reason: format!(
                        "unsupported protocol version {protocol_version}; expected {}",
                        crate::session_protocol::PROTOCOL_VERSION
                    ),
                })
                .await?;
            return Ok(());
        }
        let granted_capabilities = capability_policy.grant(requested_capabilities);
        let (session_id, generated_session) = match self.open_session(session_id).await {
            Ok(opened) => opened,
            Err(error) => {
                let reason = session_daemon_error_message(&error);
                transport
                    .send(&ServerMessage::AttachDenied {
                        code: Some(session_daemon_error_code(&error)),
                        reason,
                    })
                    .await?;
                return Ok(());
            }
        };
        let mut handshake_session =
            HandshakeSessionGuard::new(self, generated_session.then(|| session_id.clone()));
        let mut attachment = match self
            .attach_internal(
                &session_id,
                client,
                granted_capabilities.clone(),
                resume_seq.is_some(),
            )
            .await
        {
            Ok(attachment) => attachment,
            Err(error) => {
                let reason = session_daemon_error_message(&error);
                transport
                    .send(&ServerMessage::AttachDenied {
                        code: Some(session_daemon_error_code(&error)),
                        reason,
                    })
                    .await?;
                return Ok(());
            }
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(self.event_queue_capacity);
        let event_handler_tx = event_tx.clone();
        let event_lagged = Arc::new(AtomicBool::new(false));
        let handler_lagged = Arc::clone(&event_lagged);
        let observes = attachment.has_capability(ClientCapability::Observe);
        let (event_subscription, captured_snapshot) = if observes {
            let snapshot_state = Arc::clone(&attachment.entry.snapshot);
            let (subscription, snapshot) = attachment
                .core()
                .events
                .subscribe_and_capture(
                    self.max_clients_per_session.saturating_add(1),
                    Arc::new(move |seq, event| {
                        if matches!(
                            event_handler_tx.try_send(ServerMessage::Event { seq, event }),
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
                        ) {
                            handler_lagged.store(true, Ordering::Release);
                        }
                    }),
                    move || {
                        snapshot_state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .snapshot
                            .clone()
                    },
                )
                .map_err(|error| {
                    TransportError::Io(std::io::Error::other(format!(
                        "cannot subscribe session client: {error:?}"
                    )))
                })?;
            (Some(subscription), Some(snapshot))
        } else {
            (None, None)
        };
        let resume_replay = if observes {
            resume_seq.map(|last_seen_seq| attachment.core().events.replay_since(last_seen_seq))
        } else {
            None
        };
        let replay_is_available = matches!(resume_replay, Some(SessionReplay::Available { .. }));
        let snapshot = if let Some(snapshot) = captured_snapshot.filter(|_| !replay_is_available) {
            attachment.finish_handshake();
            let max_frame_bytes = limits.max_frame_bytes;
            let fitted = tokio::task::spawn_blocking(move || {
                fit_snapshot_to_frame(snapshot, max_frame_bytes)
            })
            .await
            .map_err(|error| {
                TransportError::Io(std::io::Error::other(format!(
                    "snapshot fitting task failed: {error}"
                )))
            })?;
            let admission = Arc::clone(&attachment.entry.admission).lock_owned().await;
            if attachment.entry.stopped.load(Ordering::Acquire) {
                transport
                    .send(&ServerMessage::AttachDenied {
                        code: Some(AttachDeniedCode::SessionStopped),
                        reason: "session stopped during attach".to_string(),
                    })
                    .await?;
                return Ok(());
            }
            attachment.handshake_admission = Some(admission);
            match fitted {
                Ok(snapshot) => Some(snapshot),
                Err(reason) => {
                    transport
                        .send(&ServerMessage::AttachDenied {
                            code: Some(AttachDeniedCode::SnapshotTooLarge),
                            reason,
                        })
                        .await?;
                    return Ok(());
                }
            }
        } else {
            None
        };
        let attach_sequence = match resume_replay.as_ref() {
            Some(SessionReplay::Available { latest_seq, .. }) => *latest_seq,
            Some(SessionReplay::SnapshotRequired { .. }) | None => snapshot
                .as_ref()
                .map(|snapshot| snapshot.seq)
                .unwrap_or_else(|| attachment.core().events.latest_sequence()),
        };
        transport
            .send(&ServerMessage::AttachOk {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: session_id.clone(),
                granted_capabilities,
                seq: attach_sequence,
            })
            .await?;
        attachment.finish_handshake();
        handshake_session.commit();
        let mut snapshot_sequence = snapshot.as_ref().map(|snapshot| snapshot.seq);
        if let Some(SessionReplay::Available { events, latest_seq }) = resume_replay {
            for (seq, event) in events {
                transport.send(&ServerMessage::Event { seq, event }).await?;
            }
            snapshot_sequence = Some(latest_seq);
        } else if let Some(snapshot) = snapshot {
            transport
                .send(&ServerMessage::Snapshot {
                    seq: snapshot.seq,
                    state: snapshot,
                })
                .await?;
        }
        let prompt_in_flight = Arc::new(AtomicBool::new(false));
        let mut stop_receiver = attachment.entry.stop_notifier.subscribe();
        let mut replacement_receiver = attachment.replacement_receiver.clone();
        let mut replacement_open = true;
        loop {
            tokio::select! {
                incoming = transport.recv() => match incoming? {
                    Some(_) if attachment.entry.stopped.load(Ordering::Acquire) => {
                        while let Ok(outgoing) = event_rx.try_recv() {
                            if matches!(
                                &outgoing,
                                ServerMessage::Event { seq, .. }
                                    if snapshot_sequence
                                        .is_some_and(|snapshot_seq| *seq <= snapshot_seq)
                            ) {
                                continue;
                            }
                            transport.send(&outgoing).await?;
                        }
                        transport
                            .send(&ServerMessage::Revoked {
                                code: Some(crate::session_protocol::RevocationCode::SessionStopped),
                                reason: "session stopped".to_string(),
                            })
                            .await?;
                        break;
                    }
                    Some(ClientMessage::Detach) | None => break,
                    Some(ClientMessage::Ping) => transport.send(&ServerMessage::Pong).await?,
                    Some(message @ ClientMessage::Prompt { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::Prompt { request_id, text } = message else {
                            unreachable!("matched prompt");
                        };
                        if !attachment.has_capability(ClientCapability::Prompt) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: Some(request_id),
                                    code: "capability_denied".to_string(),
                                    message: "prompt capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let core = Arc::clone(attachment.core());
                        let client_request_key =
                            client_request_key(attachment.client_id(), &request_id);
                        if core.has_completed_request(&client_request_key).await {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: Some(request_id),
                                    code: "duplicate_request".to_string(),
                                    message: "request has already completed".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        if prompt_in_flight
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: Some(request_id),
                                    code: "session_busy".to_string(),
                                    message: "a prompt is already running".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let entry = Arc::clone(&attachment.entry);
                        let prompt_events = event_tx.clone();
                        let active = Arc::clone(&prompt_in_flight);
                        let outcome_session_id = session_id.clone();
                        entry.active_prompt_tasks.fetch_add(1, Ordering::AcqRel);
                        tokio::spawn(async move {
                            let _prompt_guard = PromptInFlightGuard {
                                active,
                                entry: Arc::clone(&entry),
                            };
                            let error_request_id = request_id.clone();
                            let active_turn = {
                                let _admission = entry.admission.lock().await;
                                if entry.stopped.load(Ordering::Acquire) {
                                    Err(crate::session_core::SessionPromptError::Stopped)
                                } else {
                                    core.begin_turn()
                                        .map_err(|_| crate::session_core::SessionPromptError::Busy)
                                }
                            };
                            let result = match active_turn {
                                Ok(active_turn) => core
                                    .prompt_with_active_turn(
                                        active_turn,
                                        crate::providers::Message::user(text.clone()),
                                        text,
                                        Some(client_request_key),
                                        Some(request_id.clone()),
                                        None,
                                        || {},
                                        move |turn| {
                                            crate::session_core::canonical_assistant_outcome_with_logging(
                                                &outcome_session_id,
                                                turn,
                                            )
                                        },
                                    )
                                    .await,
                                Err(error) => Err(error),
                            };
                            if let Err(error) = result {
                                let (code, message) = prompt_error(error);
                                let _ = prompt_events
                                    .send(ServerMessage::Error {
                                        request_id: Some(error_request_id),
                                        code,
                                        message,
                                    })
                                    .await;
                            }
                        });
                    }
                    Some(message @ ClientMessage::Interrupt { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        if !attachment.has_capability(ClientCapability::Interrupt) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "capability_denied".to_string(),
                                    message: "interrupt capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let request_id = request_id(&message);
                        let changed = attachment.core().cancel_turn();
                        if let Some(request_id) = request_id {
                            transport
                                .send(&ServerMessage::CommandResult {
                                    request_id,
                                    operation: "interrupt".to_string(),
                                    changed,
                                })
                                .await?;
                        }
                    }
                    Some(message @ ClientMessage::StopSession { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        if !attachment.has_capability(ClientCapability::Stop) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "capability_denied".to_string(),
                                    message: "stop capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::StopSession { request_id } = message else {
                            unreachable!("matched stop session");
                        };
                        match self.stop_session(&session_id).await {
                            Ok(changed) => {
                                transport
                                    .send(&ServerMessage::CommandResult {
                                        request_id,
                                        operation: "stop_session".to_string(),
                                        changed,
                                    })
                                    .await?;
                            }
                            Err(error) => {
                                transport
                                    .send(&ServerMessage::Error {
                                        request_id: Some(request_id),
                                        code: "session_delete_failed".to_string(),
                                        message: error.to_string(),
                                    })
                                    .await?;
                            }
                        }
                    }
                    Some(message @ ClientMessage::ClearHistory { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        if !attachment.has_capability(ClientCapability::Configure) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "capability_denied".to_string(),
                                    message: "configure capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::ClearHistory { request_id } = message else {
                            unreachable!("matched clear history");
                        };
                        let _admission = attachment.entry.admission.lock().await;
                        match attachment.core().clear_history().await {
                            Ok(changed) => {
                                transport
                                    .send(&ServerMessage::CommandResult {
                                        request_id,
                                        operation: "clear_history".to_string(),
                                        changed,
                                    })
                                    .await?;
                            }
                            Err(crate::session_core::HistoryMutationError::Busy) => {
                                transport
                                    .send(&ServerMessage::Error {
                                        request_id: Some(request_id),
                                        code: "session_busy".to_string(),
                                        message: "history cannot be cleared while a turn is running"
                                            .to_string(),
                                    })
                                    .await?;
                            }
                        }
                    }
                    Some(message @ ClientMessage::GetUsage { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        if !attachment.has_capability(ClientCapability::Observe) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "capability_denied".to_string(),
                                    message: "observe capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::GetUsage { request_id } = message else {
                            unreachable!("matched get usage");
                        };
                        // AgentSession owns cumulative usage behind the same
                        // mutex held for a whole provider turn. Rejecting while
                        // active is explicit and avoids making this read-only
                        // query wait unpredictably for a long model/tool turn.
                        let _admission = attachment.entry.admission.lock().await;
                        if attachment.core().turn_is_active() {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: Some(request_id),
                                    code: "session_busy".to_string(),
                                    message: "usage is available after the active turn".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let usage = attachment.core().cumulative_usage().await;
                        transport
                            .send(&ServerMessage::Usage { request_id, usage })
                            .await?;
                    }
                    Some(message @ ClientMessage::ApprovalResponse { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: None,
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::ApprovalResponse {
                            approval_id,
                            decision,
                        } = message
                        else {
                            unreachable!("matched approval response");
                        };
                        if let Err(error) = attachment.core().approvals.resolve(
                            &approval_id,
                            attachment.client_id(),
                            attachment.capabilities(),
                            decision,
                        ) {
                            let (code, message) = match error {
                                crate::session_core::ApprovalError::AlreadyResolved => (
                                    "approval_already_resolved",
                                    "approval was already resolved",
                                ),
                                crate::session_core::ApprovalError::MissingCapability(_) => (
                                    "capability_denied",
                                    "approval capability was not granted",
                                ),
                                crate::session_core::ApprovalError::NotPending
                                | crate::session_core::ApprovalError::AllowAlwaysUnavailable
                                | crate::session_core::ApprovalError::IdExhausted => (
                                    "approval_rejected",
                                    "approval response was rejected",
                                ),
                            };
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: None,
                                    code: code.to_string(),
                                    message: message.to_string(),
                                })
                                .await?;
                        }
                    }
                    Some(ClientMessage::SyncRequest { last_seen_seq }) => {
                        if !attachment.has_capability(ClientCapability::Observe) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: None,
                                    code: "capability_denied".to_string(),
                                    message: "observe capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        match attachment.core().events.replay_since(last_seen_seq) {
                            SessionReplay::Available {
                                events,
                                latest_seq,
                            } => {
                                for (seq, event) in events {
                                    transport
                                        .send(&ServerMessage::Event { seq, event })
                                        .await?;
                                }
                                snapshot_sequence = Some(latest_seq);
                            }
                            SessionReplay::SnapshotRequired { .. } => {
                                let snapshot = attachment
                                    .entry
                                    .snapshot
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .snapshot
                                    .clone();
                                let max_frame_bytes = limits.max_frame_bytes;
                                let fitted = tokio::task::spawn_blocking(move || {
                                    fit_snapshot_to_frame(snapshot, max_frame_bytes)
                                })
                                .await
                                .map_err(|error| {
                                    TransportError::Io(std::io::Error::other(format!(
                                        "snapshot fitting task failed: {error}"
                                    )))
                                })?;
                                let snapshot = match fitted {
                                    Ok(snapshot) => snapshot,
                                    Err(message) => {
                                        transport
                                            .send(&ServerMessage::Error {
                                                request_id: None,
                                                code: "snapshot_too_large".to_string(),
                                                message,
                                            })
                                            .await?;
                                        continue;
                                    }
                                };
                                snapshot_sequence = Some(snapshot.seq);
                                transport
                                    .send(&ServerMessage::Snapshot {
                                        seq: snapshot.seq,
                                        state: snapshot,
                                    })
                                    .await?;
                            }
                        }
                    }
                    Some(message @ ClientMessage::SetConfig { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        if !attachment.has_capability(ClientCapability::Configure) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "capability_denied".to_string(),
                                    message: "configure capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::SetConfig {
                            request_id,
                            config_id,
                            value,
                        } = message
                        else {
                            unreachable!("matched set config");
                        };
                        let _admission = attachment.entry.admission.lock().await;
                        if let Err(error) =
                            attachment.core().apply_runtime_option(&config_id, value).await
                        {
                            let (code, message) = match error {
                                RuntimeConfigError::Busy => (
                                    "config_locked",
                                    "configuration cannot change while a turn is running",
                                ),
                                RuntimeConfigError::UnknownOption => (
                                    "unknown_config",
                                    "runtime configuration option is unknown",
                                ),
                                RuntimeConfigError::InvalidValue => (
                                    "invalid_config",
                                    "runtime configuration value is invalid",
                                ),
                                RuntimeConfigError::UnsupportedOption => (
                                    "unsupported_config",
                                    "runtime configuration option is not implemented",
                                ),
                                RuntimeConfigError::ApplyFailed(ref detail) => {
                                    tracing::warn!(
                                        target: "daimonos::session_daemon",
                                        event = "runtime_config_apply_failed",
                                        config_id,
                                        error = %detail,
                                    );
                                    (
                                        "config_apply_failed",
                                        "runtime configuration could not be applied",
                                    )
                                }
                            };
                            transport
                                .send(&ServerMessage::Error {
                                    request_id,
                                    code: code.to_string(),
                                    message: message.to_string(),
                                })
                                .await?;
                        } else if let Some(request_id) = request_id {
                            transport
                                .send(&ServerMessage::CommandResult {
                                    request_id,
                                    operation: "set_config".to_string(),
                                    changed: true,
                                })
                                .await?;
                        }
                    }
                    Some(message @ ClientMessage::ListSessions { .. }) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        if !attachment.has_capability(ClientCapability::Observe) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "capability_denied".to_string(),
                                    message: "observe capability was not granted".to_string(),
                                })
                                .await?;
                            continue;
                        }
                        let ClientMessage::ListSessions { request_id, cursor } = message else {
                            unreachable!("matched list sessions");
                        };
                        let (sessions, next_cursor) = self.list_sessions(cursor.as_deref());
                        match fit_session_list_to_frame(
                            request_id.clone(),
                            sessions,
                            next_cursor,
                            limits.max_frame_bytes,
                        ) {
                            Ok(message) => transport.send(&message).await?,
                            Err(message) => {
                                transport
                                    .send(&ServerMessage::Error {
                                        request_id: Some(request_id),
                                        code: "session_list_too_large".to_string(),
                                        message,
                                    })
                                    .await?;
                            }
                        }
                    }
                    Some(message) => {
                        if let Err(error) = limits.validate_client_message(&message) {
                            transport
                                .send(&ServerMessage::Error {
                                    request_id: request_id(&message),
                                    code: "invalid_message".to_string(),
                                    message: format!("{error:?}"),
                                })
                                .await?;
                            continue;
                        }
                        transport
                            .send(&ServerMessage::Error {
                                request_id: request_id(&message),
                                code: "unsupported_message".to_string(),
                                message: "message is not implemented by the session daemon".to_string(),
                            })
                            .await?;
                    }
                },
                outgoing = event_rx.recv() => {
                    let Some(outgoing) = outgoing else {
                        break;
                    };
                    if event_lagged.swap(false, Ordering::AcqRel) {
                        transport
                            .send(&ServerMessage::Revoked {
                                code: Some(
                                    crate::session_protocol::RevocationCode::EventQueueLagged,
                                ),
                                reason: "event queue lagged; reconnect for a full snapshot"
                                    .to_string(),
                            })
                            .await?;
                        break;
                    }
                    if matches!(
                        &outgoing,
                        ServerMessage::Event { seq, .. }
                            if snapshot_sequence.is_some_and(|snapshot_seq| *seq <= snapshot_seq)
                    ) {
                        continue;
                    }
                    transport.send(&outgoing).await?;
                    if attachment.entry.stopped.load(Ordering::Acquire) {
                        transport
                            .send(&ServerMessage::Revoked {
                                code: Some(crate::session_protocol::RevocationCode::SessionStopped),
                                reason: "session stopped".to_string(),
                            })
                            .await?;
                        break;
                    }
                }
                changed = stop_receiver.changed() => {
                    if changed.is_err() || *stop_receiver.borrow() {
                        while let Ok(outgoing) = event_rx.try_recv() {
                            if matches!(
                                &outgoing,
                                ServerMessage::Event { seq, .. }
                                    if snapshot_sequence
                                        .is_some_and(|snapshot_seq| *seq <= snapshot_seq)
                            ) {
                                continue;
                            }
                            transport.send(&outgoing).await?;
                        }
                        transport
                            .send(&ServerMessage::Revoked {
                                code: Some(crate::session_protocol::RevocationCode::SessionStopped),
                                reason: "session stopped".to_string(),
                            })
                            .await?;
                        break;
                    }
                }
                changed = replacement_receiver.changed(), if replacement_open => {
                    if *replacement_receiver.borrow() {
                        transport
                            .send(&ServerMessage::Revoked {
                                code: Some(
                                    crate::session_protocol::RevocationCode::AttachmentReplaced,
                                ),
                                reason: "attachment replaced by reconnect".to_string(),
                            })
                            .await?;
                        break;
                    }
                    if changed.is_err() {
                        replacement_open = false;
                    }
                }
            }
        }
        drop(event_subscription);
        Ok(())
    }

    #[cfg(test)]
    pub async fn serve_unix_once(
        &self,
        socket_path: PathBuf,
        max_frame_bytes: usize,
        limits: ProtocolLimits,
    ) -> Result<(), TransportError> {
        let (listener, _socket_guard, owner_uid) = bind_local_socket(&socket_path)?;
        let (stream, _) = listener.accept().await?;
        verify_peer_owner(&stream, owner_uid)?;
        let transport = UnixSocketTransport::new(stream, "local", max_frame_bytes)?;
        self.serve_client(transport, &limits).await
    }

    pub async fn serve_unix(
        self: Arc<Self>,
        socket_path: PathBuf,
        max_frame_bytes: usize,
        limits: ProtocolLimits,
        accept_error_backoff: std::time::Duration,
    ) -> Result<(), TransportError> {
        let (listener, _socket_guard, owner_uid) = bind_local_socket(&socket_path)?;
        let connection_limit = self
            .max_sessions
            .saturating_mul(self.max_clients_per_session)
            .max(1);
        let permits = Arc::new(tokio::sync::Semaphore::new(connection_limit));
        let mut idle_tick = self.idle_retention.map(tokio::time::interval);
        if let Some(tick) = idle_tick.as_mut() {
            tick.tick().await;
        }
        loop {
            let accepted = if let Some(tick) = idle_tick.as_mut() {
                tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = tick.tick() => {
                        self.evict_idle_sessions().await;
                        continue;
                    }
                }
            } else {
                listener.accept().await
            };
            let (stream, _) = match accepted {
                Ok(accepted) => accepted,
                Err(error) if accept_error_is_recoverable(&error) => {
                    tracing::warn!(
                        target: "daimonos::session_daemon",
                        event = "local_accept_retry",
                        error = %error,
                    );
                    tokio::time::sleep(accept_error_backoff).await;
                    continue;
                }
                Err(error) => return Err(TransportError::Io(error)),
            };
            if verify_peer_owner(&stream, owner_uid).is_err() {
                continue;
            }
            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                continue;
            };
            let transport = match UnixSocketTransport::new(stream, "local", max_frame_bytes) {
                Ok(transport) => transport,
                Err(_) => continue,
            };
            let daemon = Arc::clone(&self);
            daemon.active_client_tasks.fetch_add(1, Ordering::AcqRel);
            tokio::spawn(async move {
                let _client_guard = ClientTaskGuard(Arc::clone(&daemon));
                let _permit = permit;
                if let Err(error) = daemon.serve_client(transport, &limits).await {
                    tracing::warn!(
                        target: "daimonos::session_daemon",
                        event = "local_client_failed",
                        error = %error,
                    );
                }
            });
        }
    }
}

struct SocketPathGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    _lock_file: std::fs::File,
    instance_file: Option<(PathBuf, u64, u64)>,
}

struct HandshakeSessionGuard<'a> {
    daemon: &'a SessionDaemon,
    generated_session_id: Option<String>,
}

impl<'a> HandshakeSessionGuard<'a> {
    fn new(daemon: &'a SessionDaemon, generated_session_id: Option<String>) -> Self {
        Self {
            daemon,
            generated_session_id,
        }
    }

    fn commit(&mut self) {
        self.generated_session_id = None;
    }
}

impl Drop for HandshakeSessionGuard<'_> {
    fn drop(&mut self) {
        if let Some(session_id) = self.generated_session_id.take() {
            self.daemon.rollback_generated_session(&session_id);
        }
    }
}

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        if let Some(instance_file) = &self.instance_file {
            remove_matching_regular_file(instance_file);
        }
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn remove_matching_regular_file((path, device, inode): &(PathBuf, u64, u64)) {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_file() && metadata.dev() == *device && metadata.ino() == *inode {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn bind_local_socket(
    socket_path: &Path,
) -> std::io::Result<(tokio::net::UnixListener, SocketPathGuard, u32)> {
    let parent = socket_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session socket path has no parent",
        )
    })?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // SAFETY: `geteuid` has no preconditions and does not dereference pointers.
    let process_uid = unsafe { libc::geteuid() };
    let parent_metadata = std::fs::metadata(parent)?;
    if parent_metadata.uid() != process_uid || parent_metadata.permissions().mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "session socket parent must be owner-controlled and not group/world writable \
                 (owner={}, effective_uid={}, mode={:o})",
                parent_metadata.uid(),
                process_uid,
                parent_metadata.permissions().mode() & 0o777,
            ),
        ));
    }
    let mut lock_name = socket_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)?;
    // SAFETY: `flock` receives a valid open descriptor and integer flags.
    let lock_result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if lock_result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "session daemon socket lock is already held",
                )
            } else {
                error
            },
        );
    }
    if let Ok(metadata) = std::fs::symlink_metadata(socket_path) {
        if !metadata.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "refusing to replace non-socket session path",
            ));
        }
        if metadata.uid() != process_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to replace a session socket owned by another user",
            ));
        }
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "session daemon is already listening",
            ));
        }
        std::fs::remove_file(socket_path)?;
    }
    // Publish identity before bind: once connect(2) can succeed, launchers are
    // guaranteed to observe the complete atomically-renamed metadata file.
    let instance_file = write_instance_metadata(socket_path)?;
    let listener = match tokio::net::UnixListener::bind(socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            remove_matching_regular_file(&instance_file);
            return Err(error);
        }
    };
    if let Err(error) =
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
    {
        let _ = std::fs::remove_file(socket_path);
        remove_matching_regular_file(&instance_file);
        return Err(error);
    }
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = std::fs::remove_file(socket_path);
            remove_matching_regular_file(&instance_file);
            return Err(error);
        }
    };
    let guard = SocketPathGuard {
        path: socket_path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
        _lock_file: lock_file,
        instance_file: Some(instance_file),
    };
    Ok((listener, guard, metadata.uid()))
}

pub(crate) fn instance_metadata_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_os_string();
    path.push(".pid");
    PathBuf::from(path)
}

fn write_instance_metadata(socket_path: &Path) -> std::io::Result<(PathBuf, u64, u64)> {
    let path = instance_metadata_path(socket_path);
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temporary = PathBuf::from(temporary);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    let started_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let body = serde_json::to_vec(&serde_json::json!({
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "socket": socket_path,
        "started_unix_ms": started_unix_ms,
        "bootstrap_fingerprint": std::env::var(
            crate::session_bootstrap::BOOTSTRAP_FINGERPRINT_ENV
        ).ok(),
    }))
    .map_err(std::io::Error::other)?;
    if let Err(error) = file.write_all(&body).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    Ok((path, metadata.dev(), metadata.ino()))
}

fn verify_peer_owner(stream: &tokio::net::UnixStream, owner_uid: u32) -> std::io::Result<()> {
    let credentials = stream.peer_cred()?;
    if credentials.uid() != owner_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "session socket peer is owned by another user",
        ));
    }
    Ok(())
}

fn accept_error_is_recoverable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    ) || error
        .raw_os_error()
        .is_some_and(|code| code == libc::EMFILE || code == libc::ENFILE)
}

struct SnapshotState {
    snapshot: SessionSnapshot,
    max_entries: usize,
    next_transcript_id: u64,
}

impl SnapshotState {
    #[cfg(test)]
    fn new(session_id: String, max_entries: usize) -> Self {
        Self {
            snapshot: SessionSnapshot {
                session_id,
                seq: 0,
                turn_status: crate::session_protocol::TurnStatus::Idle,
                transcript: Vec::new(),
                tool_calls: Vec::new(),
                pending_approvals: Vec::new(),
                runtime_options: Vec::new(),
                context_usage: None,
                history_truncated: false,
            },
            max_entries,
            next_transcript_id: 1,
        }
    }

    fn from_snapshot(mut snapshot: SessionSnapshot, max_entries: usize) -> Self {
        let max_entries = max_entries.max(1);
        trim_oldest(&mut snapshot.transcript, max_entries);
        trim_oldest(&mut snapshot.tool_calls, max_entries);
        trim_oldest(&mut snapshot.runtime_options, max_entries);
        let next_transcript_id = snapshot
            .transcript
            .iter()
            .map(|entry| entry.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Self {
            snapshot,
            max_entries,
            next_transcript_id,
        }
    }

    fn apply(&mut self, seq: u64, event: SessionEvent) {
        self.snapshot.seq = seq;
        match event {
            SessionEvent::UserMessage { text, .. } => {
                self.push_transcript(TranscriptRole::User, text);
            }
            SessionEvent::AssistantDelta { text } => {
                self.append_transcript(TranscriptRole::Assistant, text);
            }
            SessionEvent::ThoughtDelta { text } => {
                self.append_transcript(TranscriptRole::Thought, text);
            }
            SessionEvent::AssistantDone { outcome } => {
                if self
                    .snapshot
                    .transcript
                    .last()
                    .is_none_or(|entry| entry.role != TranscriptRole::Assistant)
                {
                    self.push_transcript(TranscriptRole::Assistant, String::new());
                }
                if let Some(entry) = self.snapshot.transcript.last_mut() {
                    entry.outcome = Some(outcome);
                }
            }
            SessionEvent::SessionEnding { .. } => {}
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
                if let Some(existing) = self
                    .snapshot
                    .tool_calls
                    .iter_mut()
                    .find(|existing| existing.id == id)
                {
                    *existing = call;
                } else {
                    self.snapshot.tool_calls.push(call);
                    trim_oldest(&mut self.snapshot.tool_calls, self.max_entries);
                }
            }
            SessionEvent::ToolCallUpdated { id, status } => {
                if let Some(call) = self
                    .snapshot
                    .tool_calls
                    .iter_mut()
                    .find(|call| call.id == id)
                {
                    call.status = status;
                }
            }
            SessionEvent::ToolCallFinished { id, status, output } => {
                if let Some(call) = self
                    .snapshot
                    .tool_calls
                    .iter_mut()
                    .find(|call| call.id == id)
                {
                    call.status = status;
                    call.output = Some(output);
                }
            }
            SessionEvent::ApprovalRequested { request } => {
                if !self
                    .snapshot
                    .pending_approvals
                    .iter()
                    .any(|pending| pending.id == request.id)
                {
                    self.snapshot.pending_approvals.push(request);
                }
            }
            SessionEvent::ApprovalResolved { approval_id, .. } => {
                self.snapshot
                    .pending_approvals
                    .retain(|pending| pending.id != approval_id);
            }
            SessionEvent::ApprovalDeadlineChanged {
                approval_id,
                ineligible_deadline_unix_ms,
                paused,
            } => {
                if let Some(approval) = self
                    .snapshot
                    .pending_approvals
                    .iter_mut()
                    .find(|approval| approval.id == approval_id)
                {
                    approval.ineligible_deadline_unix_ms = Some(ineligible_deadline_unix_ms);
                    approval.deadline_paused = paused;
                }
            }
            SessionEvent::RuntimeOptionsChanged { options } => {
                self.snapshot.runtime_options = options;
                trim_oldest(&mut self.snapshot.runtime_options, self.max_entries);
            }
            SessionEvent::ContextUsageChanged { usage } => {
                self.snapshot.context_usage = Some(usage);
            }
            SessionEvent::ConversationCleared => {
                self.snapshot.transcript.clear();
                self.snapshot.tool_calls.clear();
                self.snapshot.pending_approvals.clear();
                self.snapshot.history_truncated = false;
            }
            SessionEvent::TurnStatusChanged { status } => {
                self.snapshot.turn_status = status;
            }
        }
    }

    fn push_transcript(&mut self, role: TranscriptRole, text: String) {
        let id = self.next_transcript_id;
        self.next_transcript_id = self.next_transcript_id.saturating_add(1);
        self.snapshot.transcript.push(TranscriptEntry {
            id,
            role,
            text,
            outcome: None,
        });
        trim_oldest(&mut self.snapshot.transcript, self.max_entries);
    }

    fn append_transcript(&mut self, role: TranscriptRole, text: String) {
        if let Some(last) = self.snapshot.transcript.last_mut() {
            if last.role == role {
                last.text.push_str(&text);
                return;
            }
        }
        self.push_transcript(role, text);
    }
}

fn trim_oldest<T>(entries: &mut Vec<T>, max_entries: usize) {
    let excess = entries.len().saturating_sub(max_entries);
    if excess > 0 {
        entries.drain(..excess);
    }
}

fn prompt_error(error: crate::session_core::SessionPromptError) -> (String, String) {
    match error {
        crate::session_core::SessionPromptError::Busy => (
            "session_busy".to_string(),
            "a prompt is already running".to_string(),
        ),
        crate::session_core::SessionPromptError::Stopped => (
            "session_stopped".to_string(),
            "the session has stopped".to_string(),
        ),
        crate::session_core::SessionPromptError::DuplicateRequest(request_id) => (
            "duplicate_request".to_string(),
            format!("request '{request_id}' has already completed"),
        ),
        crate::session_core::SessionPromptError::Model(message) => {
            ("model_error".to_string(), message)
        }
    }
}

struct PromptInFlightGuard {
    active: Arc<AtomicBool>,
    entry: Arc<SessionEntry>,
}

struct ClientTaskGuard(Arc<SessionDaemon>);

impl Drop for ClientTaskGuard {
    fn drop(&mut self) {
        self.0.active_client_tasks.fetch_sub(1, Ordering::AcqRel);
        self.0.client_tasks_changed.notify_waiters();
    }
}

impl Drop for PromptInFlightGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.entry
            .active_prompt_tasks
            .fetch_sub(1, Ordering::AcqRel);
        self.entry.prompt_tasks_changed.notify_waiters();
    }
}

fn fit_snapshot_to_frame(
    mut snapshot: SessionSnapshot,
    max_frame_bytes: usize,
) -> Result<SessionSnapshot, String> {
    let fits = |snapshot: &SessionSnapshot| -> Result<bool, String> {
        let message = ServerMessage::Snapshot {
            seq: snapshot.seq,
            state: snapshot.clone(),
        };
        let encoded = serde_json::to_vec(&message)
            .map_err(|error| format!("snapshot serialization failed: {error}"))?;
        Ok(encoded.len() <= max_frame_bytes)
    };
    if fits(&snapshot)? {
        return Ok(snapshot);
    }
    snapshot.history_truncated = true;
    let mut stripped_output = false;
    for call in &mut snapshot.tool_calls {
        if tool_call_is_terminal(call) {
            stripped_output |= call.output.is_some();
            call.output = None;
        }
    }
    snapshot.history_truncated |= stripped_output;
    if fits(&snapshot)? {
        return Ok(snapshot);
    }

    let transcript = std::mem::take(&mut snapshot.transcript);
    let mut low = 0;
    let mut high = transcript.len();
    while low < high {
        let middle = low + (high - low) / 2;
        snapshot.transcript = transcript[middle..].to_vec();
        if fits(&snapshot)? {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    snapshot.transcript = transcript[low..].to_vec();
    snapshot.history_truncated |= low > 0;
    if fits(&snapshot)? {
        return Ok(snapshot);
    }

    let tool_calls = std::mem::take(&mut snapshot.tool_calls);
    let terminal_count = tool_calls
        .iter()
        .filter(|call| tool_call_is_terminal(call))
        .count();
    let mut low = 0;
    let mut high = terminal_count;
    while low < high {
        let middle = low + (high - low) / 2;
        snapshot.tool_calls = tool_calls_without_oldest_terminal(&tool_calls, middle);
        if fits(&snapshot)? {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    snapshot.tool_calls = tool_calls_without_oldest_terminal(&tool_calls, low);
    snapshot.history_truncated |= low > 0;
    if fits(&snapshot)? {
        Ok(snapshot)
    } else {
        Err(format!(
            "session snapshot exceeds the configured {max_frame_bytes}-byte frame limit"
        ))
    }
}

fn tool_call_is_terminal(call: &ToolCallState) -> bool {
    matches!(
        call.status,
        ToolCallStateStatus::Completed
            | ToolCallStateStatus::Failed
            | ToolCallStateStatus::Cancelled
    )
}

fn tool_calls_without_oldest_terminal(
    tool_calls: &[ToolCallState],
    drop_count: usize,
) -> Vec<ToolCallState> {
    let mut terminal_seen = 0;
    tool_calls
        .iter()
        .filter_map(|call| {
            if tool_call_is_terminal(call) && terminal_seen < drop_count {
                terminal_seen += 1;
                None
            } else {
                Some(call.clone())
            }
        })
        .collect()
}

fn fit_session_list_to_frame(
    request_id: String,
    mut sessions: Vec<SessionListEntry>,
    mut next_cursor: Option<String>,
    max_frame_bytes: usize,
) -> Result<ServerMessage, String> {
    loop {
        let message = ServerMessage::SessionList {
            request_id: request_id.clone(),
            sessions: sessions.clone(),
            next_cursor: next_cursor.clone(),
        };
        let encoded = serde_json::to_vec(&message)
            .map_err(|error| format!("session list serialization failed: {error}"))?;
        if encoded.len() <= max_frame_bytes {
            return Ok(message);
        }
        sessions.pop();
        let Some(last) = sessions.last() else {
            return Err(format!(
                "one session entry exceeds the configured {max_frame_bytes}-byte frame limit"
            ));
        };
        next_cursor = Some(last.session_id.clone());
    }
}

fn request_id(message: &ClientMessage) -> Option<String> {
    match message {
        ClientMessage::Prompt { request_id, .. } => Some(request_id.clone()),
        ClientMessage::Interrupt { request_id } => request_id.clone(),
        ClientMessage::StopSession { request_id } => Some(request_id.clone()),
        ClientMessage::ClearHistory { request_id } => Some(request_id.clone()),
        ClientMessage::GetUsage { request_id } => Some(request_id.clone()),
        ClientMessage::ListSessions { request_id, .. } => Some(request_id.clone()),
        ClientMessage::SetConfig { request_id, .. } => request_id.clone(),
        ClientMessage::Attach { .. }
        | ClientMessage::Resume { .. }
        | ClientMessage::ApprovalResponse { .. }
        | ClientMessage::SyncRequest { .. }
        | ClientMessage::Ping
        | ClientMessage::Detach => None,
    }
}

fn session_daemon_error_message(error: &SessionDaemonError) -> String {
    match error {
        SessionDaemonError::DuplicateSession(_) => "session is already active",
        SessionDaemonError::SessionLimitReached { .. } => "session limit reached",
        SessionDaemonError::SessionNotFound(_) => "session was not found",
        SessionDaemonError::SessionStopped(_) => "session has stopped",
        SessionDaemonError::DuplicateClient(_) => "client is already attached",
        SessionDaemonError::ClientLimitReached { .. } => "client limit reached",
        SessionDaemonError::EventSubscription(_) => "session event subscription failed",
        SessionDaemonError::FactoryUnavailable => "session creation is unavailable",
        SessionDaemonError::OpenFailed(_) => "session could not be opened",
        SessionDaemonError::ShuttingDown => "session daemon is shutting down",
    }
    .to_string()
}

fn session_daemon_error_code(error: &SessionDaemonError) -> AttachDeniedCode {
    match error {
        SessionDaemonError::DuplicateSession(_) => AttachDeniedCode::SessionAlreadyActive,
        SessionDaemonError::SessionLimitReached { .. } => AttachDeniedCode::SessionLimitReached,
        SessionDaemonError::SessionNotFound(_) => AttachDeniedCode::SessionNotFound,
        SessionDaemonError::SessionStopped(_) => AttachDeniedCode::SessionStopped,
        SessionDaemonError::DuplicateClient(_) => AttachDeniedCode::DuplicateClient,
        SessionDaemonError::ClientLimitReached { .. } => AttachDeniedCode::ClientLimitReached,
        SessionDaemonError::EventSubscription(_) => AttachDeniedCode::EventSubscriptionFailed,
        SessionDaemonError::FactoryUnavailable => AttachDeniedCode::FactoryUnavailable,
        // Task 1335's version/corrupt/workspace store errors subdivide this
        // coarse prerequisite code in the coordinated protocol-v3 change.
        SessionDaemonError::OpenFailed(_) => AttachDeniedCode::SessionOpenFailed,
        SessionDaemonError::ShuttingDown => AttachDeniedCode::DaemonShuttingDown,
    }
}

fn client_request_key(client_id: &str, request_id: &str) -> String {
    format!("{}:{client_id}{request_id}", client_id.len())
}

pub struct AttachedSession {
    entry: Arc<SessionEntry>,
    client_id: String,
    capabilities: Vec<ClientCapability>,
    attachment_id: u64,
    replacement_receiver: tokio::sync::watch::Receiver<bool>,
    handshake_admission: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl AttachedSession {
    pub fn core(&self) -> &Arc<SessionCore> {
        &self.entry.core
    }

    pub fn has_capability(&self, capability: ClientCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn capabilities(&self) -> &[ClientCapability] {
        &self.capabilities
    }

    fn finish_handshake(&mut self) {
        self.handshake_admission = None;
    }
}

impl Drop for AttachedSession {
    fn drop(&mut self) {
        let mut clients = self
            .entry
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if clients
            .get(&self.client_id)
            .is_none_or(|client| client.attachment_id != self.attachment_id)
        {
            return;
        }
        clients.remove(&self.client_id);
        let approve_once_count = clients
            .values()
            .filter(|client| client.capabilities.contains(&ClientCapability::ApproveOnce))
            .count();
        let approve_always_count = clients
            .values()
            .filter(|client| {
                client
                    .capabilities
                    .contains(&ClientCapability::ApproveAlways)
            })
            .count();
        self.entry
            .core
            .approvals
            .set_eligible_client_counts(approve_once_count, approve_always_count);
        drop(clients);
        if approve_once_count == 0 && approve_always_count == 0 {
            let clients_empty = self
                .entry
                .clients
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty();
            if clients_empty {
                *self
                    .entry
                    .last_detached_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(tokio::time::Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::agent::{AgentConfig, AgentSession};
    use crate::providers::{
        CompleteOpts, ContentBlock, Context, LlmProvider, LlmResponse, StopReason, StreamEvent,
        Usage,
    };
    use crate::session::Session;
    use crate::session_core::{
        ApprovalBroker, CanonicalToolLifecycle, SessionCompaction, SessionCore, SessionEventRouter,
    };
    use crate::session_protocol::{ClientCapability, ClientInfo, ClientKind};
    use crate::{
        client_transport::in_memory_transport_pair,
        session_protocol::{ClientMessage, ProtocolLimits, ServerMessage},
    };

    struct StaticProvider;

    #[async_trait::async_trait]
    impl LlmProvider for StaticProvider {
        async fn complete(&self, _context: &Context, _options: &CompleteOpts) -> LlmResponse {
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::Text("pong".to_string())],
                stop_reason: StopReason::EndTurn,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            }
        }
    }

    struct BlockingProvider;

    #[async_trait::async_trait]
    impl LlmProvider for BlockingProvider {
        async fn complete(&self, _context: &Context, _options: &CompleteOpts) -> LlmResponse {
            std::future::pending().await
        }
    }

    struct TestSessionFactory;

    #[async_trait::async_trait]
    impl SessionFactory for TestSessionFactory {
        async fn open(
            &self,
            _session_id: &str,
            _mode: SessionOpenMode,
        ) -> Result<Arc<SessionCore>, String> {
            Ok(test_core())
        }
    }

    struct CountingSessionFactory {
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct BlockingOpenFactory {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl SessionFactory for BlockingOpenFactory {
        async fn open(
            &self,
            _session_id: &str,
            _mode: SessionOpenMode,
        ) -> Result<Arc<SessionCore>, String> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(test_core())
        }
    }

    #[async_trait::async_trait]
    impl SessionFactory for CountingSessionFactory {
        async fn open(
            &self,
            _session_id: &str,
            _mode: SessionOpenMode,
        ) -> Result<Arc<SessionCore>, String> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(test_core())
        }
    }

    fn test_core_with_provider(provider: Box<dyn LlmProvider>) -> Arc<SessionCore> {
        test_core_with_options(provider, None, 0)
    }

    fn test_core_with_persistence(
        provider: Box<dyn LlmProvider>,
        persistence: Option<crate::session_core::SessionPersistence>,
    ) -> Arc<SessionCore> {
        test_core_with_options(provider, persistence, 0)
    }

    fn test_core_with_replay(max_replay_events: usize) -> Arc<SessionCore> {
        test_core_with_options(Box::new(StaticProvider), None, max_replay_events)
    }

    fn test_core_with_options(
        provider: Box<dyn LlmProvider>,
        persistence: Option<crate::session_core::SessionPersistence>,
        max_replay_events: usize,
    ) -> Arc<SessionCore> {
        let config = Arc::new(crate::config::Config::default());
        let workspace = std::env::temp_dir();
        let events = Arc::new(SessionEventRouter::new_with_replay(None, max_replay_events));
        let approvals = Arc::new(ApprovalBroker::new(false));
        let tool_lifecycle = Arc::new(CanonicalToolLifecycle::new(
            Arc::clone(&events),
            Arc::clone(&approvals),
            Arc::new(crate::safety::SafetyPolicy::default()),
            4,
        ));
        let stream_events = Arc::clone(&events);
        Arc::new(SessionCore::new(
            AgentSession::new(
                provider,
                Session::new(workspace.clone(), config),
                AgentConfig {
                    opts: CompleteOpts {
                        model: "test-model".to_string(),
                        ..CompleteOpts::default()
                    },
                    on_stream_event: Some(Box::new(move |event| {
                        let event = match event {
                            StreamEvent::TextDelta(text) => SessionEvent::AssistantDelta { text },
                            StreamEvent::ThinkingDelta(text) => SessionEvent::ThoughtDelta { text },
                        };
                        let _ = stream_events.emit(event);
                    })),
                    ..AgentConfig::default()
                },
            ),
            "test-model".to_string(),
            workspace,
            SessionCompaction::new(None, false),
            HashMap::new(),
            approvals,
            persistence,
            events,
            tool_lifecycle,
        ))
    }

    fn test_core() -> Arc<SessionCore> {
        test_core_with_provider(Box::new(StaticProvider))
    }

    #[tokio::test]
    async fn canonical_clear_resets_memory_persistence_and_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::session_store::SessionStore::new(directory.path().to_path_buf());
        let core = test_core_with_options(
            Box::new(StaticProvider),
            Some(crate::session_core::SessionPersistence::new(
                "session-1",
                store.clone(),
            )),
            8,
        );
        {
            let mut session = core.session.lock().await;
            session.set_history(vec![
                crate::providers::Message::user("before"),
                crate::providers::Message::assistant("answer"),
            ]);
        }
        core.client_user_message_ids
            .lock()
            .await
            .push("request-1".to_string());
        core.assistant_outcomes
            .lock()
            .unwrap()
            .push(crate::session_protocol::AssistantOutcome::Completed);
        core.persist(
            "test-model",
            core.session.lock().await.history(),
            &["request-1".to_string()],
        );

        let lifecycle = core.lifecycle.lock().await;
        let clearing = {
            let core = Arc::clone(&core);
            tokio::spawn(async move { core.clear_history().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !core.mutation_is_active() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("clear acquires the mutation permit before awaiting lifecycle");
        assert!(matches!(
            core.begin_turn(),
            Err(crate::session_core::TurnError::Busy)
        ));
        drop(lifecycle);
        assert!(clearing.await.unwrap().unwrap());
        assert!(core.session.lock().await.history().is_empty());
        assert!(core.client_user_message_ids.lock().await.is_empty());
        assert!(core.assistant_outcomes.lock().unwrap().is_empty());
        let persisted = store.load("session-1").expect("cleared session persists");
        assert!(persisted.messages.is_empty());
        assert!(persisted.client_user_message_ids.is_empty());
        assert!(persisted.assistant_outcomes.is_empty());
        let snapshot = core.initial_snapshot("session-1".to_string(), 32).await;
        assert!(snapshot.transcript.is_empty());
        assert!(snapshot.tool_calls.is_empty());
        assert!(matches!(
            core.events.replay_since(0),
            SessionReplay::Available { events, .. }
                if events.iter().any(|(_, event)|
                    matches!(event, SessionEvent::ConversationCleared))
        ));
    }

    fn terminal_client() -> ClientInfo {
        ClientInfo {
            id: "terminal-1".to_string(),
            kind: ClientKind::Terminal,
            label: "local terminal".to_string(),
        }
    }

    fn protocol_limits() -> ProtocolLimits {
        ProtocolLimits {
            max_frame_bytes: 4_096,
            max_prompt_bytes: 1024,
            max_label_bytes: 128,
            max_identifier_bytes: 128,
            max_ticket_bytes: 256,
            max_runtime_value_bytes: 256,
            max_capabilities: 8,
        }
    }

    #[test]
    fn client_request_dedup_keys_are_unambiguous() {
        assert_ne!(
            client_request_key("a:b", "c"),
            client_request_key("a", "b:c")
        );
        assert_eq!(
            client_request_key("a:b", "c"),
            client_request_key("a:b", "c")
        );
    }

    #[tokio::test]
    async fn detaching_last_client_keeps_daemon_owned_session_alive() {
        let daemon = SessionDaemon::new(4, 2, 8, 32);
        let core = test_core();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .expect("create session");

        let mut attachment = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![
                    ClientCapability::Observe,
                    ClientCapability::Prompt,
                    ClientCapability::Interrupt,
                    ClientCapability::ApproveOnce,
                    ClientCapability::ApproveAlways,
                ],
            )
            .await
            .expect("attach local terminal");
        attachment.finish_handshake();
        assert!(Arc::ptr_eq(attachment.core(), &core));
        assert_eq!(daemon.attached_client_count("session-1"), Some(1));

        drop(attachment);

        assert_eq!(daemon.attached_client_count("session-1"), Some(0));
        assert!(Arc::ptr_eq(
            &daemon
                .session("session-1")
                .expect("session survives detach"),
            &core
        ));
    }

    #[tokio::test]
    async fn stopping_a_session_releases_its_bounded_registry_slot() {
        let daemon = SessionDaemon::new(1, 2, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .expect("first session");
        assert_eq!(
            daemon.create_session("session-2".to_string(), test_core()),
            Err(SessionDaemonError::SessionLimitReached { max: 1 })
        );

        assert!(daemon.stop_session("session-1").await.unwrap());
        assert!(daemon.session("session-1").is_none());
        daemon
            .create_session("session-2".to_string(), test_core())
            .expect("stopped session releases capacity");
    }

    #[tokio::test]
    async fn explicit_stop_deletes_persisted_session() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::session_store::SessionStore::new(directory.path().to_path_buf());
        let core = test_core_with_persistence(
            Box::new(StaticProvider),
            Some(crate::session_core::SessionPersistence::new(
                "session-1",
                store.clone(),
            )),
        );
        core.persist(
            "test-model",
            &[crate::providers::Message::user("persisted")],
            &[],
        );
        assert!(store.load("session-1").is_some());
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), core)
            .unwrap();

        assert!(daemon.stop_session("session-1").await.unwrap());

        assert!(store.load("session-1").is_none());
    }

    #[tokio::test]
    async fn daemon_shutdown_preserves_persisted_session() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::session_store::SessionStore::new(directory.path().to_path_buf());
        let core = test_core_with_persistence(
            Box::new(StaticProvider),
            Some(crate::session_core::SessionPersistence::new(
                "session-1",
                store.clone(),
            )),
        );
        core.persist(
            "test-model",
            &[crate::providers::Message::user("persisted")],
            &[],
        );
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), core)
            .unwrap();

        daemon.shutdown().await;

        assert!(store.load("session-1").is_some());
    }

    #[tokio::test]
    async fn explicit_stop_cannot_race_persistence_recreation() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::session_store::SessionStore::new(directory.path().to_path_buf());
        let core = test_core_with_persistence(
            Box::new(StaticProvider),
            Some(crate::session_core::SessionPersistence::new(
                "session-1",
                store.clone(),
            )),
        );
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        let initial_snapshot = core.initial_snapshot("session-1".to_string(), 32).await;
        daemon
            .create_session_with_snapshot(
                "session-1".to_string(),
                Arc::clone(&core),
                initial_snapshot,
            )
            .unwrap();
        let persister = std::thread::spawn(move || {
            for _ in 0..100 {
                core.persist(
                    "test-model",
                    &[crate::providers::Message::user("racing")],
                    &[],
                );
            }
        });

        assert!(daemon.stop_session("session-1").await.unwrap());
        persister.join().unwrap();

        assert!(store.load("session-1").is_none());
    }

    #[tokio::test]
    async fn daemon_shutdown_stops_every_live_session() {
        let daemon = SessionDaemon::new(2, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        daemon
            .create_session("session-2".to_string(), test_core())
            .unwrap();

        daemon.shutdown().await;

        assert!(daemon.session("session-1").is_none());
        assert!(daemon.session("session-2").is_none());
    }

    #[tokio::test]
    async fn stopping_session_revokes_attached_client() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let client_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            client_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));

        assert!(daemon.stop_session("session-1").await.unwrap());
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Event {
                event: SessionEvent::SessionEnding { .. },
                ..
            })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Revoked {
                code: Some(crate::session_protocol::RevocationCode::SessionStopped),
                reason,
            }) if reason == "session stopped"
        ));
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stopped_session_rejects_prompt_racing_end_event() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        let core = test_core();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let client_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            client_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));

        assert!(daemon.stop_session("session-1").await.unwrap());
        client
            .send(ClientMessage::Prompt {
                request_id: "too-late".to_string(),
                text: "must not run".to_string(),
            })
            .await
            .unwrap();
        while !matches!(
            client.recv().await,
            Some(ServerMessage::Revoked { .. }) | None
        ) {}
        serve.await.unwrap().unwrap();

        assert!(core.session.lock().await.history().is_empty());
    }

    #[tokio::test]
    async fn cancelled_stop_keeps_session_reachable() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let mut attachment = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe],
            )
            .await
            .unwrap();
        let stop_daemon = Arc::clone(&daemon);
        let stop = tokio::spawn(async move { stop_daemon.stop_session("session-1").await });
        tokio::task::yield_now().await;
        assert!(!stop.is_finished(), "stop should wait for attach admission");

        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        assert!(daemon.session("session-1").is_some());

        attachment.finish_handshake();
        drop(attachment);
        assert!(daemon.stop_session("session-1").await.unwrap());
    }

    #[tokio::test]
    async fn granted_stop_message_ends_daemon_owned_session() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let client_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            client_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe, ClientCapability::Stop],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
        client
            .send(ClientMessage::StopSession {
                request_id: "stop-1".to_string(),
            })
            .await
            .unwrap();

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::CommandResult {
                request_id,
                operation,
                changed: true,
            }) if request_id == "stop-1" && operation == "stop_session"
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Event {
                event: SessionEvent::SessionEnding { .. },
                ..
            })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Revoked { .. })
        ));
        serve.await.unwrap().unwrap();
        assert!(daemon.session("session-1").is_none());
    }

    #[tokio::test]
    async fn attached_clients_are_bounded_and_detach_releases_capacity() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .expect("session");
        let mut first = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe],
            )
            .await
            .expect("first client");
        first.finish_handshake();
        let second = ClientInfo {
            id: "terminal-2".to_string(),
            kind: ClientKind::Headless,
            label: "headless".to_string(),
        };
        assert_eq!(
            session_daemon_error_code(&SessionDaemonError::ClientLimitReached { max: 1 }),
            AttachDeniedCode::ClientLimitReached
        );
        assert!(matches!(
            daemon
                .attach("session-1", second.clone(), vec![ClientCapability::Observe])
                .await,
            Err(SessionDaemonError::ClientLimitReached { max: 1 })
        ));

        drop(first);
        daemon
            .attach("session-1", second, vec![ClientCapability::Observe])
            .await
            .expect("detach releases client capacity");
    }

    #[tokio::test]
    async fn resume_replaces_same_client_without_old_drop_removing_new_attachment() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let mut first = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe],
            )
            .await
            .unwrap();
        first.finish_handshake();
        let mut replacement = daemon
            .attach_internal(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe],
                true,
            )
            .await
            .unwrap();
        replacement.finish_handshake();

        assert!(*first.replacement_receiver.borrow());
        drop(first);
        assert_eq!(daemon.attached_client_count("session-1"), Some(1));
        drop(replacement);
        assert_eq!(daemon.attached_client_count("session-1"), Some(0));
    }

    #[tokio::test]
    async fn detach_and_reattach_updates_approval_eligibility_without_denial() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        let core = test_core();
        let registered = core
            .approvals
            .register(crate::session_protocol::ApprovalRequest::unassigned(
                "tool-1",
                "exec",
                "cargo test",
                false,
            ))
            .unwrap();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let mut attachment = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe, ClientCapability::ApproveOnce],
            )
            .await
            .unwrap();
        attachment.finish_handshake();
        assert!(core.approvals.has_eligible_client(&registered.request.id));

        drop(attachment);
        assert!(!core.approvals.has_eligible_client(&registered.request.id));
        assert_eq!(core.approvals.pending().len(), 1);

        let mut reattached = daemon
            .attach(
                "session-1",
                ClientInfo {
                    id: "terminal-2".to_string(),
                    kind: ClientKind::Terminal,
                    label: "reattached".to_string(),
                },
                vec![ClientCapability::ApproveOnce],
            )
            .await
            .unwrap();
        reattached.finish_handshake();
        assert!(core.approvals.has_eligible_client(&registered.request.id));
        core.approvals.cancel_all("test_cleanup");
    }

    #[tokio::test]
    async fn repeated_attach_detach_does_not_accumulate_clients() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();

        for index in 0..100 {
            let mut client = terminal_client();
            client.id = format!("terminal-{index}");
            let mut attachment = daemon
                .attach("session-1", client, vec![ClientCapability::Observe])
                .await
                .unwrap();
            attachment.finish_handshake();
            drop(attachment);
        }

        assert_eq!(daemon.attached_client_count("session-1"), Some(0));
    }

    #[tokio::test]
    async fn connection_rejects_any_first_message_other_than_attach() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        let (transport, mut client) = in_memory_transport_pair(2, "test client");
        client.send(ClientMessage::Ping).await.unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .expect("protocol rejection is delivered");

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachDenied {
                code: Some(AttachDeniedCode::InvalidMessage),
                reason,
            })
                if reason.contains("first message must be attach")
        ));
    }

    #[tokio::test]
    async fn client_without_observe_receives_no_snapshot_or_events() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(4, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Prompt],
            })
            .await
            .unwrap();
        client.send(ClientMessage::Ping).await.unwrap();
        client.send(ClientMessage::Detach).await.unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .unwrap();

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert_eq!(client.recv().await, Some(ServerMessage::Pong));
    }

    #[tokio::test]
    async fn capability_policy_intersects_requested_grants() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::Prompt,
                    ClientCapability::Stop,
                ],
            })
            .await
            .unwrap();
        client
            .send(ClientMessage::Prompt {
                request_id: "forged".to_string(),
                text: "must not run".to_string(),
            })
            .await
            .unwrap();
        client
            .send(ClientMessage::SetConfig {
                request_id: Some("forged-config".to_string()),
                config_id: "model".to_string(),
                value: crate::session_protocol::RuntimeValue::String("forged".to_string()),
            })
            .await
            .unwrap();
        client.send(ClientMessage::Detach).await.unwrap();

        daemon
            .serve_client_with_policy(
                transport,
                &protocol_limits(),
                &CapabilityPolicy::allowing([ClientCapability::Observe]),
            )
            .await
            .unwrap();

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk {
                granted_capabilities,
                ..
            }) if granted_capabilities == vec![ClientCapability::Observe]
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Error { request_id, code, .. })
                if code == "capability_denied" && request_id.as_deref() == Some("forged")
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Error { request_id, code, .. })
                if code == "capability_denied"
                    && request_id.as_deref() == Some("forged-config")
        ));
    }

    #[tokio::test]
    async fn set_config_applies_model_and_projects_runtime_options() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        let core = test_core();
        core.set_runtime_options(vec![
            crate::session_protocol::RuntimeOption::select(
                "model",
                "Model",
                crate::session_protocol::RuntimeValue::String("test-model".to_string()),
                vec![
                    crate::session_protocol::RuntimeChoice::new("test-model", "test-model"),
                    crate::session_protocol::RuntimeChoice::new("model-b", "model-b"),
                ],
            ),
            crate::session_protocol::RuntimeOption::select(
                "thinking",
                "Thinking",
                crate::session_protocol::RuntimeValue::String("medium".to_string()),
                vec![
                    crate::session_protocol::RuntimeChoice::new("medium", "medium"),
                    crate::session_protocol::RuntimeChoice::new("high", "high"),
                ],
            ),
        ]);
        core.context_windows
            .lock()
            .await
            .insert("model-b".to_string(), 200);
        core.publish_context_usage(crate::session_protocol::ContextUsage::new(
            50,
            Some(100),
            0,
            None,
        ));
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let serve_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            serve_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::Prompt,
                    ClientCapability::Configure,
                ],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { state, .. })
                if state.runtime_options[0].value
                    == crate::session_protocol::RuntimeValue::String("test-model".to_string())
        ));

        client
            .send(ClientMessage::SetConfig {
                request_id: Some("model-change".to_string()),
                config_id: "model".to_string(),
                value: crate::session_protocol::RuntimeValue::String("model-b".to_string()),
            })
            .await
            .unwrap();
        let mut saw_context = false;
        let mut saw_model = false;
        let mut saw_result = false;
        for _ in 0..3 {
            match client.recv().await {
                Some(ServerMessage::Event {
                    event: SessionEvent::ContextUsageChanged { usage },
                    ..
                }) => {
                    saw_context = usage.prompt_tokens == 50
                        && usage.model_context_window == Some(200)
                        && usage.estimated;
                }
                Some(ServerMessage::Event {
                    event: SessionEvent::RuntimeOptionsChanged { options },
                    ..
                }) => {
                    saw_model = options[0].value
                        == crate::session_protocol::RuntimeValue::String("model-b".to_string());
                }
                Some(ServerMessage::CommandResult {
                    request_id,
                    operation,
                    changed: true,
                }) => {
                    saw_result = request_id == "model-change" && operation == "set_config";
                }
                _ => {}
            }
        }
        assert!(saw_context && saw_model && saw_result);
        client
            .send(ClientMessage::SetConfig {
                request_id: Some("thinking-change".to_string()),
                config_id: "thinking".to_string(),
                value: crate::session_protocol::RuntimeValue::String("high".to_string()),
            })
            .await
            .unwrap();
        let mut saw_thinking = false;
        let mut saw_result = false;
        for _ in 0..2 {
            match client.recv().await {
                Some(ServerMessage::Event {
                    event: SessionEvent::RuntimeOptionsChanged { options },
                    ..
                }) => {
                    saw_thinking = options.iter().any(|option| {
                        option.id == "thinking"
                            && option.value
                                == crate::session_protocol::RuntimeValue::String("high".to_string())
                    });
                }
                Some(ServerMessage::CommandResult {
                    request_id,
                    operation,
                    changed: true,
                }) => {
                    saw_result = request_id == "thinking-change" && operation == "set_config";
                }
                _ => {}
            }
        }
        assert!(saw_thinking && saw_result);

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
        assert_eq!(core.current_model(), "model-b");
    }

    #[tokio::test]
    async fn clear_and_usage_commands_are_daemon_authoritative() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 16, 32));
        let directory = tempfile::tempdir().unwrap();
        let store = crate::session_store::SessionStore::new(directory.path().to_path_buf());
        let core = test_core_with_persistence(
            Box::new(StaticProvider),
            Some(crate::session_core::SessionPersistence::new(
                "session-1",
                store.clone(),
            )),
        );
        core.session
            .lock()
            .await
            .set_history(vec![crate::providers::Message::user("before")]);
        core.persist("test-model", core.session.lock().await.history(), &[]);
        let initial_snapshot = core.initial_snapshot("session-1".to_string(), 32).await;
        daemon
            .create_session_with_snapshot(
                "session-1".to_string(),
                Arc::clone(&core),
                initial_snapshot,
            )
            .unwrap();

        let (transport, mut client) = in_memory_transport_pair(16, "test client");
        let serve_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            serve_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::Configure,
                ],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        let Some(ServerMessage::Snapshot { state, .. }) = client.recv().await else {
            panic!("expected snapshot");
        };
        let initial_transcript_id = state.transcript[0].id;

        client
            .send(ClientMessage::GetUsage {
                request_id: "usage-1".to_string(),
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Usage { request_id, usage })
                if request_id == "usage-1"
                    && usage.input == 0
                    && usage.output == 0
                    && usage.cost_usd_micros == 0
        ));

        client
            .send(ClientMessage::ClearHistory {
                request_id: "clear-1".to_string(),
            })
            .await
            .unwrap();
        let mut saw_clear = false;
        let mut saw_context = false;
        let mut saw_result = false;
        for _ in 0..3 {
            match client.recv().await {
                Some(ServerMessage::Event {
                    event: SessionEvent::ConversationCleared,
                    ..
                }) => saw_clear = true,
                Some(ServerMessage::Event {
                    event: SessionEvent::ContextUsageChanged { usage },
                    ..
                }) => saw_context = usage.prompt_tokens == 0,
                Some(ServerMessage::CommandResult {
                    request_id,
                    operation,
                    changed: true,
                }) => {
                    saw_result = request_id == "clear-1" && operation == "clear_history";
                }
                _ => {}
            }
        }
        assert!(saw_clear && saw_context && saw_result);
        assert!(core.session.lock().await.history().is_empty());
        assert!(store.load("session-1").unwrap().messages.is_empty());
        let _ = core.events.emit(SessionEvent::UserMessage {
            text: "after".to_string(),
            request_id: None,
        });
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Event {
                event: SessionEvent::UserMessage { text, .. },
                ..
            }) if text == "after"
        ));
        let snapshot = daemon
            .sessions
            .lock()
            .unwrap()
            .get("session-1")
            .unwrap()
            .snapshot
            .lock()
            .unwrap()
            .snapshot
            .clone();
        assert!(snapshot.transcript[0].id > initial_transcript_id);

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn attach_waits_for_the_same_admission_gate_as_clear() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let entry = daemon
            .sessions
            .lock()
            .unwrap()
            .get("session-1")
            .unwrap()
            .clone();
        let admission = Arc::clone(&entry.admission).lock_owned().await;
        let attaching = {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                daemon
                    .attach(
                        "session-1",
                        terminal_client(),
                        vec![ClientCapability::Observe],
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!attaching.is_finished());
        drop(admission);
        let attached = attaching.await.unwrap().unwrap();
        drop(attached);
    }

    #[tokio::test]
    async fn runtime_config_rejects_invalid_values_and_active_turn_changes() {
        let core = test_core();
        core.set_runtime_options(vec![crate::session_protocol::RuntimeOption::select(
            "model",
            "Model",
            crate::session_protocol::RuntimeValue::String("test-model".to_string()),
            vec![
                crate::session_protocol::RuntimeChoice::new("test-model", "test-model"),
                crate::session_protocol::RuntimeChoice::new("model-b", "model-b"),
            ],
        )]);
        assert!(matches!(
            core.apply_runtime_option(
                "model",
                crate::session_protocol::RuntimeValue::String("not-listed".to_string()),
            )
            .await,
            Err(RuntimeConfigError::InvalidValue)
        ));

        core.session
            .lock()
            .await
            .set_history(vec![crate::providers::Message::user(
                "restored conversation content",
            )]);
        core.apply_runtime_option(
            "model",
            crate::session_protocol::RuntimeValue::String("model-b".to_string()),
        )
        .await
        .expect("idle model change");
        let snapshot = core.initial_snapshot("session".to_string(), 32).await;
        let usage = snapshot.context_usage.expect("estimated context usage");
        assert!(usage.estimated);
        assert!(
            usage.prompt_tokens > 0,
            "restored history must not render as empty"
        );

        let _turn = core.begin_turn().expect("begin turn");
        assert!(matches!(
            core.apply_runtime_option(
                "model",
                crate::session_protocol::RuntimeValue::String("test-model".to_string()),
            )
            .await,
            Err(RuntimeConfigError::Busy)
        ));
    }

    #[tokio::test]
    async fn stopped_client_without_observe_is_revoked_without_sending() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(4, "test client");
        let serve_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            serve_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Prompt],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));

        assert!(daemon.stop_session("session-1").await.unwrap());

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), client.recv()).await,
            Ok(Some(ServerMessage::Revoked { .. }))
        ));
        serve.await.unwrap().unwrap();
    }

    #[test]
    fn oversized_snapshot_is_trimmed_to_transport_frame() {
        let mut snapshot = SnapshotState::new("session-1".to_string(), 100).snapshot;
        snapshot.transcript = (0..100)
            .map(|id| TranscriptEntry {
                id,
                role: TranscriptRole::Assistant,
                text: "x".repeat(100),
                outcome: None,
            })
            .collect();

        let fitted = fit_snapshot_to_frame(snapshot, 1_024).unwrap();
        let encoded = serde_json::to_vec(&ServerMessage::Snapshot {
            seq: fitted.seq,
            state: fitted.clone(),
        })
        .unwrap();

        assert!(encoded.len() <= 1_024);
        assert!(fitted.transcript.len() < 100);
        assert!(fitted.history_truncated);
    }

    #[tokio::test]
    async fn sync_snapshot_is_frame_bounded_and_suppresses_covered_events() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 512, 512));
        let core = test_core();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(512, "test client");
        let client_daemon = Arc::clone(&daemon);
        let mut limits = protocol_limits();
        limits.max_frame_bytes = 1_024;
        let serve =
            tokio::spawn(async move { client_daemon.serve_client(transport, &limits).await });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
        for index in 0..100 {
            core.events
                .emit(SessionEvent::UserMessage {
                    text: format!("message-{index}-{}", "x".repeat(100)),
                    request_id: None,
                })
                .unwrap();
        }
        client
            .send(ClientMessage::SyncRequest { last_seen_seq: 0 })
            .await
            .unwrap();

        let snapshot = loop {
            match client.recv().await.unwrap() {
                ServerMessage::Snapshot { state, .. } => break state,
                ServerMessage::Event { .. } => {}
                other => panic!("unexpected sync response: {other:?}"),
            }
        };
        let encoded = serde_json::to_vec(&ServerMessage::Snapshot {
            seq: snapshot.seq,
            state: snapshot.clone(),
        })
        .unwrap();
        assert!(encoded.len() <= 1_024);
        client.send(ClientMessage::Ping).await.unwrap();
        assert_eq!(client.recv().await, Some(ServerMessage::Pong));

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sync_replays_retained_event_suffix_without_snapshot() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 16, 32));
        let core = test_core_with_replay(4);
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(16, "test client");
        let client_daemon = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            client_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
        for text in ["one", "two"] {
            core.events
                .emit(SessionEvent::AssistantDelta {
                    text: text.to_string(),
                })
                .unwrap();
        }
        for expected in [1, 2] {
            assert!(matches!(
                client.recv().await,
                Some(ServerMessage::Event { seq, .. }) if seq == expected
            ));
        }

        client
            .send(ClientMessage::SyncRequest { last_seen_seq: 0 })
            .await
            .unwrap();
        for expected in [1, 2] {
            assert!(matches!(
                client.recv().await,
                Some(ServerMessage::Event { seq, .. }) if seq == expected
            ));
        }

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn resume_handshake_replays_from_client_watermark() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 16, 32));
        let core = test_core_with_replay(4);
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        for text in ["one", "two", "three"] {
            core.events
                .emit(SessionEvent::AssistantDelta {
                    text: text.to_string(),
                })
                .unwrap();
        }
        let (transport, mut client) = in_memory_transport_pair(16, "test client");
        client
            .send(ClientMessage::Resume {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: "session-1".to_string(),
                last_seen_seq: 1,
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        client.send(ClientMessage::Detach).await.unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .unwrap();

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { seq: 3, .. })
        ));
        for expected in [2, 3] {
            assert!(matches!(
                client.recv().await,
                Some(ServerMessage::Event { seq, .. }) if seq == expected
            ));
        }
        assert!(!matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
    }

    #[tokio::test]
    async fn attach_existing_session_then_detach_leaves_session_running() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        let core = test_core();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(4, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
            })
            .await
            .unwrap();
        client.send(ClientMessage::Detach).await.unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .expect("client lifecycle");

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk {
                session_id,
                granted_capabilities,
                seq: 0,
                ..
            }) if session_id == "session-1"
                && granted_capabilities
                    == vec![ClientCapability::Observe, ClientCapability::Prompt]
        ));
        assert_eq!(daemon.attached_client_count("session-1"), Some(0));
        assert!(Arc::ptr_eq(
            &daemon.session("session-1").expect("session survives"),
            &core
        ));
    }

    #[tokio::test]
    async fn attached_client_receives_canonical_session_events() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        let core = test_core();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(4, "test client");
        let daemon_task = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            daemon_task
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { seq: 0, .. })
        ));

        core.events
            .emit(crate::session_protocol::SessionEvent::AssistantDelta {
                text: "streamed".to_string(),
            })
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), client.recv()).await,
            Ok(Some(ServerMessage::Event {
                seq: 1,
                event: crate::session_protocol::SessionEvent::AssistantDelta { text }
            })) if text == "streamed"
        ));

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn prompt_capability_runs_turn_and_streams_canonical_outcome() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let daemon_task = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            daemon_task
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe, ClientCapability::Prompt],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        client
            .send(ClientMessage::Prompt {
                request_id: "prompt-1".to_string(),
                text: "ping".to_string(),
            })
            .await
            .unwrap();

        let mut events = Vec::new();
        for _ in 0..8 {
            let message = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
                .await
                .expect("prompt event timeout")
                .expect("daemon response");
            if let ServerMessage::Event { event, .. } = message {
                let done = matches!(
                    event,
                    crate::session_protocol::SessionEvent::AssistantDone { .. }
                );
                events.push(event);
                if done {
                    break;
                }
            }
        }
        assert!(events.iter().any(|event| matches!(
            event,
            crate::session_protocol::SessionEvent::UserMessage {
                text,
                request_id: Some(request_id),
            } if text == "ping" && request_id == "prompt-1"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::session_protocol::SessionEvent::AssistantDelta { text } if text == "pong"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::session_protocol::SessionEvent::AssistantDone {
                outcome: crate::session_protocol::AssistantOutcome::Completed
            }
        )));

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn interrupt_capability_cancels_active_turn_without_stopping_session() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        daemon
            .create_session(
                "session-1".to_string(),
                test_core_with_provider(Box::new(BlockingProvider)),
            )
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let daemon_task = Arc::clone(&daemon);
        let serve = tokio::spawn(async move {
            daemon_task
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::Prompt,
                    ClientCapability::Interrupt,
                ],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        client
            .send(ClientMessage::Prompt {
                request_id: "prompt-1".to_string(),
                text: "wait".to_string(),
            })
            .await
            .unwrap();

        loop {
            let Some(ServerMessage::Event { event, .. }) = client.recv().await else {
                continue;
            };
            if matches!(
                event,
                crate::session_protocol::SessionEvent::TurnStatusChanged {
                    status: crate::session_protocol::TurnStatus::Running
                }
            ) {
                break;
            }
        }
        client
            .send(ClientMessage::Interrupt {
                request_id: Some("prompt-1".to_string()),
            })
            .await
            .unwrap();

        let mut cancelled = false;
        for _ in 0..4 {
            let message = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
                .await
                .expect("cancellation event timeout")
                .expect("daemon response");
            if matches!(
                message,
                ServerMessage::Event {
                    event: crate::session_protocol::SessionEvent::TurnStatusChanged {
                        status: crate::session_protocol::TurnStatus::Cancelled
                    },
                    ..
                }
            ) {
                cancelled = true;
                break;
            }
        }
        assert!(cancelled);
        assert!(daemon.session("session-1").is_some());

        client.send(ClientMessage::Detach).await.unwrap();
        serve.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn approval_response_resolves_broker_with_attached_client_identity() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        let core = test_core();
        let registered = core
            .approvals
            .register(crate::session_protocol::ApprovalRequest::unassigned(
                "tool-1",
                "exec",
                "cargo test",
                false,
            ))
            .unwrap();
        let approval_id = registered.request.id.clone();
        daemon
            .create_session("session-1".to_string(), core)
            .unwrap();
        let (transport, client) = in_memory_transport_pair(4, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![
                    ClientCapability::Observe,
                    ClientCapability::ApproveOnce,
                ],
            })
            .await
            .unwrap();
        client
            .send(ClientMessage::ApprovalResponse {
                approval_id,
                decision: crate::session_protocol::ApprovalDecision::AllowOnce,
            })
            .await
            .unwrap();
        client.send(ClientMessage::Detach).await.unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .unwrap();
        let resolution = registered.receiver.await.unwrap();
        assert_eq!(
            resolution.decision,
            crate::session_protocol::ApprovalDecision::AllowOnce
        );
        assert_eq!(resolution.resolved_by, "terminal-1");
    }

    #[tokio::test]
    async fn reconnect_receives_equivalent_full_snapshot() {
        let daemon = SessionDaemon::new(1, 1, 8, 32);
        let core = test_core();
        daemon
            .create_session("session-1".to_string(), Arc::clone(&core))
            .unwrap();
        core.events
            .emit(crate::session_protocol::SessionEvent::UserMessage {
                text: "ping".to_string(),
                request_id: None,
            })
            .unwrap();
        core.events
            .emit(crate::session_protocol::SessionEvent::AssistantDelta {
                text: "po".to_string(),
            })
            .unwrap();
        core.events
            .emit(crate::session_protocol::SessionEvent::AssistantDelta {
                text: "ng".to_string(),
            })
            .unwrap();
        core.events
            .emit(crate::session_protocol::SessionEvent::AssistantDone {
                outcome: crate::session_protocol::AssistantOutcome::Errored {
                    context_overflow: false,
                    message: "provider unavailable".to_string(),
                },
            })
            .unwrap();

        let mut snapshots = Vec::new();
        for client_number in 1..=2 {
            let (transport, mut client) = in_memory_transport_pair(4, "test client");
            client
                .send(ClientMessage::Attach {
                    protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                    session_id: Some("session-1".to_string()),
                    ticket: None,
                    client: ClientInfo {
                        id: format!("terminal-{client_number}"),
                        kind: ClientKind::Terminal,
                        label: "local terminal".to_string(),
                    },
                    requested_capabilities: vec![ClientCapability::Observe],
                })
                .await
                .unwrap();
            client.send(ClientMessage::Detach).await.unwrap();
            daemon
                .serve_client(transport, &protocol_limits())
                .await
                .unwrap();
            assert!(matches!(
                client.recv().await,
                Some(ServerMessage::AttachOk { seq: 4, .. })
            ));
            let Ok(Some(ServerMessage::Snapshot { seq, state })) =
                tokio::time::timeout(std::time::Duration::from_secs(1), client.recv()).await
            else {
                panic!("attach must return a full snapshot");
            };
            assert_eq!(seq, 4);
            snapshots.push(state);
        }

        assert_eq!(snapshots[0], snapshots[1]);
        assert_eq!(snapshots[0].transcript.len(), 2);
        assert_eq!(snapshots[0].transcript[0].text, "ping");
        assert_eq!(snapshots[0].transcript[1].text, "pong");
        assert!(matches!(
            snapshots[0].transcript[1].outcome,
            Some(crate::session_protocol::AssistantOutcome::Errored { .. })
        ));
    }

    #[tokio::test]
    async fn attach_without_session_id_creates_daemon_owned_session() {
        let daemon = SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            None,
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        );
        let (transport, mut client) = in_memory_transport_pair(4, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        client.send(ClientMessage::Detach).await.unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .unwrap();
        let Some(ServerMessage::AttachOk { session_id, .. }) = client.recv().await else {
            panic!("new session attach");
        };
        assert!(!session_id.is_empty());
        assert!(daemon.session(&session_id).is_some());
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { state, .. }) if state.session_id == session_id
        ));
    }

    #[tokio::test]
    async fn full_registry_rejects_before_constructing_session() {
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let daemon = SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            None,
            50,
            std::time::Duration::from_secs(1),
            Arc::new(CountingSessionFactory {
                opens: Arc::clone(&opens),
            }),
        );
        daemon
            .create_session("existing".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(2, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();

        daemon
            .serve_client(transport, &protocol_limits())
            .await
            .unwrap();

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachDenied { .. })
        ));
        assert_eq!(opens.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn failed_new_session_handshake_releases_registry_slot() {
        let daemon = SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            None,
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        );
        let (transport, mut client) = in_memory_transport_pair(2, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        let mut limits = protocol_limits();
        limits.max_frame_bytes = 1;

        daemon.serve_client(transport, &limits).await.unwrap();

        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachDenied { .. })
        ));
        assert_eq!(daemon.session_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_preempts_blocked_session_factory() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let daemon = Arc::new(SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            None,
            50,
            std::time::Duration::from_millis(50),
            Arc::new(BlockingOpenFactory {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }),
        ));
        let (transport, client) = in_memory_transport_pair(2, "test client");
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: None,
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        let serve_daemon = Arc::clone(&daemon);
        let serving = tokio::spawn(async move {
            serve_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        started.notified().await;

        tokio::time::timeout(std::time::Duration::from_millis(100), daemon.shutdown())
            .await
            .expect("shutdown must not wait for factory open");
        release.notify_one();
        serving.await.unwrap().unwrap();
        assert_eq!(daemon.session_count(), 0);
    }

    #[test]
    fn session_listing_is_bounded_and_cursor_paginated() {
        let daemon = SessionDaemon::with_factory(
            3,
            1,
            8,
            32,
            None,
            1,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        );
        daemon
            .create_session("session-a".to_string(), test_core())
            .unwrap();
        daemon
            .create_session("session-b".to_string(), test_core())
            .unwrap();

        let (first, cursor) = daemon.list_sessions(None);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].session_id, "session-a");
        let (second, next) = daemon.list_sessions(cursor.as_deref());
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].session_id, "session-b");
        assert!(next.is_none());
    }

    #[tokio::test]
    async fn idle_retention_evicts_detached_session_and_recovers_capacity() {
        let daemon = SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            Some(std::time::Duration::from_millis(10)),
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        );
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let mut attachment = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe],
            )
            .await
            .unwrap();
        attachment.finish_handshake();
        drop(attachment);

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        daemon.evict_idle_sessions().await;

        assert!(daemon.session("session-1").is_none());
        daemon
            .create_session("session-2".to_string(), test_core())
            .expect("idle eviction releases capacity");
    }

    #[tokio::test]
    async fn idle_retention_rechecks_concurrent_reattach() {
        let daemon = SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            Some(std::time::Duration::from_millis(10)),
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        );
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let mut first = daemon
            .attach(
                "session-1",
                terminal_client(),
                vec![ClientCapability::Observe],
            )
            .await
            .unwrap();
        first.finish_handshake();
        drop(first);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut second = daemon
            .attach(
                "session-1",
                ClientInfo {
                    id: "terminal-2".to_string(),
                    kind: ClientKind::Terminal,
                    label: "reattached".to_string(),
                },
                vec![ClientCapability::Observe],
            )
            .await
            .unwrap();
        second.finish_handshake();

        daemon.evict_idle_sessions().await;

        assert!(daemon.session("session-1").is_some());
    }

    #[tokio::test]
    async fn idle_retention_preserves_pending_prompt_admission() {
        let daemon = SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            Some(std::time::Duration::from_millis(10)),
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        );
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let entry = daemon
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get("session-1")
            .cloned()
            .unwrap();
        *entry
            .last_detached_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(tokio::time::Instant::now() - std::time::Duration::from_millis(20));
        entry.active_prompt_tasks.store(1, Ordering::Release);

        daemon.evict_idle_sessions().await;
        assert!(daemon.session("session-1").is_some());

        entry.active_prompt_tasks.store(0, Ordering::Release);
        daemon.evict_idle_sessions().await;
        assert!(daemon.session("session-1").is_none());
    }

    #[tokio::test]
    async fn detached_retention_does_not_timeout_attached_client() {
        let daemon = Arc::new(SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            Some(std::time::Duration::from_millis(10)),
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        ));
        daemon
            .create_session("session-1".to_string(), test_core())
            .unwrap();
        let (transport, mut client) = in_memory_transport_pair(8, "test client");
        let serve_daemon = Arc::clone(&daemon);
        let serving = tokio::spawn(async move {
            serve_daemon
                .serve_client(transport, &protocol_limits())
                .await
        });
        client
            .send(ClientMessage::Attach {
                protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                session_id: Some("session-1".to_string()),
                ticket: None,
                client: terminal_client(),
                requested_capabilities: vec![ClientCapability::Observe],
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::AttachOk { .. })
        ));
        assert!(matches!(
            client.recv().await,
            Some(ServerMessage::Snapshot { .. })
        ));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        client.send(ClientMessage::Ping).await.unwrap();
        assert_eq!(client.recv().await, Some(ServerMessage::Pong));
        client.send(ClientMessage::Detach).await.unwrap();
        serving.await.unwrap().unwrap();
    }

    #[test]
    fn session_list_is_trimmed_to_transport_frame() {
        let sessions = (0..20)
            .map(|index| SessionListEntry {
                session_id: format!("session-{index:04}"),
                active: true,
                attached_clients: 0,
            })
            .collect();
        let message = fit_session_list_to_frame("list-1".to_string(), sessions, None, 300).unwrap();
        assert!(serde_json::to_vec(&message).unwrap().len() <= 300);
        let ServerMessage::SessionList {
            sessions,
            next_cursor,
            ..
        } = message
        else {
            panic!("session list");
        };
        assert!(!sessions.is_empty());
        assert!(next_cursor.is_some());
    }

    #[tokio::test]
    async fn unix_listener_uses_owner_only_socket_and_cleans_up_on_exit() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = directory.path().join("session.sock");
        let daemon = Arc::new(SessionDaemon::with_factory(
            1,
            1,
            8,
            32,
            None,
            50,
            std::time::Duration::from_secs(1),
            Arc::new(TestSessionFactory),
        ));
        let listener_daemon = Arc::clone(&daemon);
        let listener_path = socket_path.clone();
        let serve = tokio::spawn(async move {
            listener_daemon
                .serve_unix_once(listener_path, 4_096, protocol_limits())
                .await
        });
        let mut attempts = 0;
        let stream = loop {
            match tokio::net::UnixStream::connect(&socket_path).await {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    attempts += 1;
                    if serve.is_finished() {
                        panic!("listener exited before bind: {:?}", serve.await);
                    }
                    assert!(attempts < 1_000, "session socket was not created");
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                Err(error) => panic!("connect to session socket: {error}"),
            }
        };
        assert_eq!(
            std::fs::metadata(&socket_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let instance_path = instance_metadata_path(&socket_path);
        let instance_metadata = std::fs::metadata(&instance_path).unwrap();
        assert_eq!(instance_metadata.permissions().mode() & 0o777, 0o600);
        let instance: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&instance_path).unwrap()).unwrap();
        assert_eq!(instance["pid"], std::process::id());
        assert_eq!(instance["socket"], socket_path.to_string_lossy().as_ref());
        let (reader, mut writer) = stream.into_split();
        let attach = ClientMessage::Attach {
            protocol_version: crate::session_protocol::PROTOCOL_VERSION,
            session_id: None,
            ticket: None,
            client: terminal_client(),
            requested_capabilities: vec![ClientCapability::Observe],
        };
        writer
            .write_all(format!("{}\n", serde_json::to_string(&attach).unwrap()).as_bytes())
            .await
            .unwrap();
        writer.write_all(b"{\"type\":\"detach\"}\n").await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(line.trim_end()).unwrap(),
            ServerMessage::AttachOk { .. }
        ));
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(line.trim_end()).unwrap(),
            ServerMessage::Snapshot { .. }
        ));

        serve.await.unwrap().unwrap();
        assert!(!socket_path.exists());
        assert!(!instance_path.exists());
    }

    #[tokio::test]
    async fn unix_listener_rejects_group_or_world_writable_parent() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = match bind_local_socket(&directory.path().join("session.sock")) {
            Ok(_) => panic!("unsafe parent permissions must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn unix_listener_lock_allows_only_one_daemon_owner() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = directory.path().join("session.sock");
        let (listener, guard, _) = bind_local_socket(&socket_path).unwrap();

        let error = match bind_local_socket(&socket_path) {
            Ok(_) => panic!("second daemon must not replace the live socket"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);

        drop(listener);
        drop(guard);
        assert!(bind_local_socket(&socket_path).is_ok());
    }

    #[tokio::test]
    async fn unix_listener_replaces_stale_owner_socket_under_lock() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = directory.path().join("session.sock");
        let stale = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(stale);
        assert!(std::os::unix::net::UnixStream::connect(&socket_path).is_err());

        let (listener, guard, _) = bind_local_socket(&socket_path).unwrap();
        assert!(std::os::unix::net::UnixStream::connect(&socket_path).is_ok());
        drop(listener);
        drop(guard);
        assert!(!socket_path.exists());
        assert!(!instance_metadata_path(&socket_path).exists());
    }

    #[test]
    fn accept_error_classification_retries_resource_pressure_only() {
        assert!(accept_error_is_recoverable(
            &std::io::Error::from_raw_os_error(libc::EMFILE)
        ));
        assert!(accept_error_is_recoverable(&std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "signal",
        )));
        assert!(!accept_error_is_recoverable(&std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "listener invalid",
        )));
    }

    #[tokio::test]
    async fn cancelling_unix_server_removes_owned_socket() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = directory.path().join("session.sock");
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        let server = tokio::spawn(Arc::clone(&daemon).serve_unix(
            socket_path.clone(),
            4_096,
            protocol_limits(),
            std::time::Duration::from_millis(1),
        ));
        for _ in 0..1_000 {
            if socket_path.exists() {
                break;
            }
            assert!(!server.is_finished(), "server exited before binding");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(socket_path.exists());

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        assert!(!socket_path.exists());
    }
}
