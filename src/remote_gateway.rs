use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::client_transport::{ClientTransport, TransportError};
use crate::remote_auth::{AuthError, PairingAuthority, PendingPairing, TicketGrant};
use crate::session_daemon::SessionDaemon;
use crate::session_protocol::{
    ClientCapability, ClientInfo, ClientKind, ClientMessage, ProtocolLimits, ServerMessage,
};

#[derive(Debug, Clone)]
pub struct RemoteGatewayConfig {
    pub allowed_origins: HashSet<String>,
    pub max_frame_bytes: usize,
    pub pairing_wait: Duration,
    pub auth_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub max_messages_per_second: u32,
    pub max_connections: usize,
    pub admission_attempts_per_minute: u32,
    pub max_unauthenticated_per_ip: usize,
    pub trust_proxy_headers: bool,
    pub max_admission_peers: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RemoteClientFrame {
    Pair {
        claim: String,
        device_public_key: String,
        label: String,
        requested_capabilities: Vec<ClientCapability>,
    },
    Authenticate {
        ticket: String,
        device_public_key: String,
        signature: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RemoteServerFrame {
    Challenge {
        challenge: String,
    },
    PairingPending {
        pairing: PendingPairing,
    },
    PairingApproved {
        grant: TicketGrant,
    },
    PairingCommitted {
        device_id: String,
    },
    Authenticated {
        device_id: String,
        capabilities: Vec<ClientCapability>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Default)]
struct ControllerState {
    current: Option<String>,
}

struct ControllerGuard {
    state: Arc<Mutex<ControllerState>>,
    device_id: String,
}

impl Drop for ControllerGuard {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.current.as_deref() == Some(self.device_id.as_str()) {
            state.current = None;
        }
    }
}

#[derive(Clone)]
struct GatewayState {
    daemon: Arc<SessionDaemon>,
    authenticator: Arc<PairingAuthority>,
    limits: ProtocolLimits,
    config: RemoteGatewayConfig,
    controller: Arc<Mutex<ControllerState>>,
    connections: Arc<tokio::sync::Semaphore>,
    admission: Arc<Mutex<AdmissionState>>,
}

#[derive(Default)]
struct AdmissionState {
    peers: HashMap<IpAddr, PeerAdmission>,
}

struct PeerAdmission {
    window_started: std::time::Instant,
    attempts: u32,
    unauthenticated: usize,
}

struct AdmissionGuard {
    state: Arc<Mutex<AdmissionState>>,
    peer: IpAddr,
    unauthenticated: bool,
}

impl AdmissionGuard {
    fn mark_authenticated(&mut self) {
        if self.unauthenticated {
            decrement_unauthenticated(&self.state, self.peer);
            self.unauthenticated = false;
        }
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if self.unauthenticated {
            decrement_unauthenticated(&self.state, self.peer);
        }
    }
}

pub struct RemoteGateway {
    state: GatewayState,
}

impl RemoteGateway {
    pub fn new(
        daemon: Arc<SessionDaemon>,
        authenticator: Arc<PairingAuthority>,
        limits: ProtocolLimits,
        config: RemoteGatewayConfig,
    ) -> Self {
        let connections = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
        Self {
            state: GatewayState {
                daemon,
                authenticator,
                limits,
                config,
                controller: Arc::new(Mutex::new(ControllerState::default())),
                connections,
                admission: Arc::new(Mutex::new(AdmissionState::default())),
            },
        }
    }

    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let app = Router::new()
            .route("/v2/ws", get(websocket_upgrade))
            .with_state(self.state);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    }
}

async fn websocket_upgrade(
    State(state): State<GatewayState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&headers, &state.config.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin is not allowed").into_response();
    }
    let admission_peer = match admission_peer(&headers, peer, state.config.trust_proxy_headers) {
        Ok(peer) => peer,
        Err(status) => return (status, "invalid proxy client address").into_response(),
    };
    let admission = match admit_peer(
        &state.admission,
        admission_peer,
        state.config.admission_attempts_per_minute,
        state.config.max_unauthenticated_per_ip,
        state.config.max_admission_peers,
    ) {
        Ok(admission) => admission,
        Err(status) => return (status, "remote admission limit reached").into_response(),
    };
    let permit = match Arc::clone(&state.connections).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "remote connection limit reached",
            )
                .into_response();
        }
    };
    upgrade
        .max_message_size(state.config.max_frame_bytes)
        .max_frame_size(state.config.max_frame_bytes)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            serve_socket(state, socket, admission).await;
        })
        .into_response()
}

fn admission_peer(
    headers: &HeaderMap,
    peer: SocketAddr,
    trust_proxy_headers: bool,
) -> Result<IpAddr, StatusCode> {
    if !trust_proxy_headers {
        return Ok(peer.ip());
    }
    if !peer.ip().is_loopback() {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(forwarded) = headers.get("x-forwarded-for") else {
        return Ok(peer.ip());
    };
    let forwarded = forwarded.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
    forwarded
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)
}

fn origin_allowed(headers: &HeaderMap, allowed_origins: &HashSet<String>) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    origin
        .to_str()
        .ok()
        .is_some_and(|origin| allowed_origins.contains(origin))
}

fn admit_peer(
    state: &Arc<Mutex<AdmissionState>>,
    peer: IpAddr,
    attempts_per_minute: u32,
    max_unauthenticated: usize,
    max_peers: usize,
) -> Result<AdmissionGuard, StatusCode> {
    let mut admission = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = std::time::Instant::now();
    admission.peers.retain(|_, peer_state| {
        peer_state.unauthenticated > 0
            || now.duration_since(peer_state.window_started) < Duration::from_secs(60)
    });
    if !admission.peers.contains_key(&peer) && admission.peers.len() >= max_peers {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let peer_state = admission.peers.entry(peer).or_insert(PeerAdmission {
        window_started: now,
        attempts: 0,
        unauthenticated: 0,
    });
    if now.duration_since(peer_state.window_started) >= Duration::from_secs(60) {
        peer_state.window_started = now;
        peer_state.attempts = 0;
    }
    if peer_state.attempts >= attempts_per_minute {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    if peer_state.unauthenticated >= max_unauthenticated {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    peer_state.attempts = peer_state.attempts.saturating_add(1);
    peer_state.unauthenticated = peer_state.unauthenticated.saturating_add(1);
    Ok(AdmissionGuard {
        state: Arc::clone(state),
        peer,
        unauthenticated: true,
    })
}

fn decrement_unauthenticated(state: &Arc<Mutex<AdmissionState>>, peer: IpAddr) {
    let mut admission = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(peer_state) = admission.peers.get_mut(&peer) {
        peer_state.unauthenticated = peer_state.unauthenticated.saturating_sub(1);
    }
}

async fn serve_socket(state: GatewayState, socket: WebSocket, mut admission: AdmissionGuard) {
    let transport = WebSocketTransport::new(
        socket,
        state.config.max_frame_bytes,
        state.config.heartbeat_interval,
        state.config.heartbeat_timeout,
        state.config.max_messages_per_second,
        "remote",
    );
    let challenge = random_challenge();
    if transport
        .send_frame(&RemoteServerFrame::Challenge {
            challenge: challenge.clone(),
        })
        .await
        .is_err()
    {
        return;
    }
    let first = match tokio::time::timeout(state.config.auth_timeout, transport.recv_remote_frame())
        .await
    {
        Ok(Ok(Some(frame))) => frame,
        _ => return,
    };
    match first {
        RemoteClientFrame::Pair {
            claim,
            device_public_key,
            label,
            requested_capabilities,
        } => {
            serve_pairing(
                &state,
                &transport,
                &claim,
                &device_public_key,
                label,
                requested_capabilities,
            )
            .await;
        }
        RemoteClientFrame::Authenticate {
            ticket,
            device_public_key,
            signature,
        } => {
            let authenticated = match state.authenticator.authenticate(
                &ticket,
                &device_public_key,
                &challenge,
                &signature,
            ) {
                Ok(authenticated) => authenticated,
                Err(error) => {
                    let _ = transport
                        .send_error("authentication_failed", auth_error_message(&error))
                        .await;
                    return;
                }
            };
            admission.mark_authenticated();
            let mut revoked = match state
                .authenticator
                .revocation_receiver(&authenticated.device_id)
            {
                Some(receiver) => receiver,
                None => return,
            };
            if transport
                .send_frame(&RemoteServerFrame::Authenticated {
                    device_id: authenticated.device_id.clone(),
                    capabilities: authenticated.capabilities.clone(),
                })
                .await
                .is_err()
            {
                return;
            }
            let first_message = tokio::select! {
                message = transport.recv() => match message {
                    Ok(Some(message)) => bind_remote_identity(
                        message,
                        &authenticated.device_id,
                        &authenticated.label,
                    ),
                    _ => return,
                },
                _ = revoked.changed() => return,
            };
            let Some(first_message) = first_message else {
                let _ = transport
                    .send_error(
                        "invalid_attach",
                        "first authenticated frame must attach or resume".to_string(),
                    )
                    .await;
                return;
            };
            let requested_capabilities = match &first_message {
                ClientMessage::Attach {
                    requested_capabilities,
                    ..
                }
                | ClientMessage::Resume {
                    requested_capabilities,
                    ..
                } => requested_capabilities
                    .iter()
                    .copied()
                    .filter(|capability| authenticated.capabilities.contains(capability))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let controller_guard = match acquire_controller(
                &state.controller,
                &authenticated.device_id,
                &requested_capabilities,
            ) {
                Ok(guard) => guard,
                Err(()) => {
                    let _ = transport
                        .send_error(
                            "controller_busy",
                            "another remote controller is attached".to_string(),
                        )
                        .await;
                    return;
                }
            };
            let transport = PrefixedTransport::new(transport, first_message);
            let serve = state.daemon.serve_client_with_capabilities(
                transport,
                &state.limits,
                authenticated.capabilities,
            );
            tokio::pin!(serve);
            tokio::select! {
                _ = &mut serve => {}
                changed = revoked.changed() => {
                    if changed.is_ok() && *revoked.borrow() {
                        tracing::info!(
                            event = "remote_device_revoked",
                            device_id = %authenticated.device_id,
                        );
                    }
                }
            }
            drop(controller_guard);
        }
    }
}

async fn serve_pairing(
    state: &GatewayState,
    transport: &WebSocketTransport,
    claim: &str,
    device_public_key: &str,
    label: String,
    requested_capabilities: Vec<ClientCapability>,
) {
    if label.len() > state.limits.max_label_bytes
        || requested_capabilities.len() > state.limits.max_capabilities
    {
        let _ = transport
            .send_error(
                "invalid_pairing",
                "pairing metadata exceeds configured limits".to_string(),
            )
            .await;
        return;
    }
    let label = sanitize_remote_label(&label);
    let pending = match state.authenticator.submit_pairing(
        claim,
        device_public_key,
        label,
        requested_capabilities,
    ) {
        Ok(pending) => pending,
        Err(error) => {
            let _ = transport
                .send_error("pairing_rejected", auth_error_message(&error))
                .await;
            return;
        }
    };
    if transport
        .send_frame(&RemoteServerFrame::PairingPending {
            pairing: pending.clone(),
        })
        .await
        .is_err()
    {
        state.authenticator.abort_pairing(&pending.id);
        return;
    }
    let deadline = tokio::time::Instant::now() + state.config.pairing_wait;
    loop {
        match state.authenticator.pairing_grant(&pending.id) {
            Ok(Some(grant)) => {
                let sent = transport
                    .send_frame(&RemoteServerFrame::PairingApproved { grant })
                    .await;
                if sent.is_ok() {
                    state.authenticator.finish_pairing(&pending.id);
                    let _ = transport
                        .send_frame(&RemoteServerFrame::PairingCommitted {
                            device_id: pending.device_id.clone(),
                        })
                        .await;
                } else {
                    state.authenticator.abort_pairing(&pending.id);
                }
                return;
            }
            Ok(None) if tokio::time::Instant::now() < deadline => {}
            Ok(None) => {
                state.authenticator.abort_pairing(&pending.id);
                let _ = transport
                    .send_error("pairing_timeout", "local approval timed out".to_string())
                    .await;
                return;
            }
            Err(error) => {
                state.authenticator.finish_pairing(&pending.id);
                let _ = transport
                    .send_error("pairing_denied", auth_error_message(&error))
                    .await;
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            incoming = transport.recv_text() => {
                state.authenticator.abort_pairing(&pending.id);
                if matches!(incoming, Ok(Some(_))) {
                    let _ = transport
                        .send_error(
                            "unexpected_pairing_frame",
                            "pairing waits only for local consent".to_string(),
                        )
                        .await;
                }
                return;
            }
        }
    }
}

fn sanitize_remote_label(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '\u{202a}'
                        ..= '\u{202e}'
                            | '\u{2066}'..='\u{2069}'
                            | '\u{200e}'
                            | '\u{200f}'
                )
        })
        .collect();
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        "Remote device".to_string()
    } else {
        sanitized.to_string()
    }
}

fn bind_remote_identity(
    message: ClientMessage,
    device_id: &str,
    label: &str,
) -> Option<ClientMessage> {
    let client = ClientInfo {
        id: format!("remote:{device_id}"),
        kind: ClientKind::Android,
        label: label.to_string(),
    };
    match message {
        ClientMessage::Attach {
            protocol_version,
            session_id,
            ticket: _,
            requested_capabilities,
            ..
        } => Some(ClientMessage::Attach {
            protocol_version,
            session_id,
            ticket: None,
            client,
            requested_capabilities,
        }),
        ClientMessage::Resume {
            protocol_version,
            session_id,
            last_seen_seq,
            ticket: _,
            requested_capabilities,
            ..
        } => Some(ClientMessage::Resume {
            protocol_version,
            session_id,
            last_seen_seq,
            ticket: None,
            client,
            requested_capabilities,
        }),
        _ => None,
    }
}

fn acquire_controller(
    state: &Arc<Mutex<ControllerState>>,
    device_id: &str,
    capabilities: &[ClientCapability],
) -> Result<Option<ControllerGuard>, ()> {
    if !capabilities.iter().any(is_control_capability) {
        return Ok(None);
    }
    let mut controller = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if controller.current.is_some() {
        return Err(());
    }
    controller.current = Some(device_id.to_string());
    Ok(Some(ControllerGuard {
        state: Arc::clone(state),
        device_id: device_id.to_string(),
    }))
}

fn is_control_capability(capability: &ClientCapability) -> bool {
    matches!(
        capability,
        ClientCapability::Prompt
            | ClientCapability::Configure
            | ClientCapability::Interrupt
            | ClientCapability::ApproveOnce
            | ClientCapability::ApproveAlways
            | ClientCapability::Stop
    )
}

fn random_challenge() -> String {
    format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
}

fn auth_error_message(error: &AuthError) -> String {
    match error {
        AuthError::UnknownClaim | AuthError::ExpiredClaim => "pairing claim is invalid or expired",
        AuthError::InvalidDeviceKey => "device key is invalid",
        AuthError::PairingNotFound => "pairing request was not found",
        AuthError::PairingAlreadyResolved => "pairing request was already resolved",
        AuthError::CapabilitiesNotRequested => "approval exceeds requested capabilities",
        AuthError::DeviceLimitReached => "paired device limit reached",
        AuthError::UnknownTicket | AuthError::DeviceKeyMismatch | AuthError::InvalidSignature => {
            "device authentication failed"
        }
    }
    .to_string()
}

struct WebSocketTransport {
    socket: tokio::sync::Mutex<WebSocket>,
    max_frame_bytes: usize,
    peer_label: String,
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
    rate: Mutex<MessageRate>,
}

struct MessageRate {
    window_started: std::time::Instant,
    count: u32,
    max_per_second: u32,
}

impl MessageRate {
    fn check(&mut self, now: std::time::Instant) -> Result<(), TransportError> {
        if now.duration_since(self.window_started) >= Duration::from_secs(1) {
            self.window_started = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        if self.count > self.max_per_second {
            return Err(TransportError::Backpressure);
        }
        Ok(())
    }
}

impl WebSocketTransport {
    fn new(
        socket: WebSocket,
        max_frame_bytes: usize,
        heartbeat_interval: Duration,
        heartbeat_timeout: Duration,
        max_messages_per_second: u32,
        peer_label: impl Into<String>,
    ) -> Self {
        Self {
            socket: tokio::sync::Mutex::new(socket),
            max_frame_bytes,
            peer_label: peer_label.into(),
            heartbeat_interval,
            heartbeat_timeout,
            rate: Mutex::new(MessageRate {
                window_started: std::time::Instant::now(),
                count: 0,
                max_per_second: max_messages_per_second,
            }),
        }
    }

    async fn send_frame<T: Serialize>(&self, frame: &T) -> Result<(), TransportError> {
        let text = serde_json::to_string(frame)?;
        if text.len() > self.max_frame_bytes {
            return Err(TransportError::FrameTooLarge {
                max_bytes: self.max_frame_bytes,
            });
        }
        self.socket
            .lock()
            .await
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| TransportError::Io(std::io::Error::other(error)))
    }

    async fn send_error(&self, code: &'static str, message: String) -> Result<(), TransportError> {
        self.send_frame(&RemoteServerFrame::Error {
            code: code.to_string(),
            message,
        })
        .await
    }

    async fn recv_text(&self) -> Result<Option<String>, TransportError> {
        loop {
            let message = match tokio::time::timeout(
                self.heartbeat_interval,
                self.socket.lock().await.recv(),
            )
            .await
            {
                Ok(message) => message,
                Err(_) => {
                    self.socket
                        .lock()
                        .await
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .map_err(|error| TransportError::Io(std::io::Error::other(error)))?;
                    tokio::time::timeout(self.heartbeat_timeout, self.socket.lock().await.recv())
                        .await
                        .map_err(|_| {
                            TransportError::Io(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "remote WebSocket heartbeat timed out",
                            ))
                        })?
                }
            }
            .transpose()
            .map_err(|error| TransportError::Io(std::io::Error::other(error)))?;
            if message.is_some() {
                self.check_rate()?;
            }
            match message {
                Some(Message::Text(text)) => {
                    if text.len() > self.max_frame_bytes {
                        return Err(TransportError::FrameTooLarge {
                            max_bytes: self.max_frame_bytes,
                        });
                    }
                    return Ok(Some(text.to_string()));
                }
                Some(Message::Close(_)) | None => return Ok(None),
                Some(Message::Ping(payload)) => {
                    self.socket
                        .lock()
                        .await
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| TransportError::Io(std::io::Error::other(error)))?;
                }
                Some(Message::Pong(_)) => {}
                Some(Message::Binary(_)) => {
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "binary WebSocket frames are not supported",
                    )));
                }
            }
        }
    }

    fn check_rate(&self) -> Result<(), TransportError> {
        let mut rate = self
            .rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rate.check(std::time::Instant::now())
    }

    async fn recv_remote_frame(&self) -> Result<Option<RemoteClientFrame>, TransportError> {
        self.recv_text()
            .await?
            .map(|text| serde_json::from_str(&text).map_err(TransportError::from))
            .transpose()
    }
}

#[async_trait]
impl ClientTransport for WebSocketTransport {
    async fn send(&self, message: &ServerMessage) -> Result<(), TransportError> {
        self.send_frame(message).await
    }

    async fn recv(&self) -> Result<Option<ClientMessage>, TransportError> {
        self.recv_text()
            .await?
            .map(|text| serde_json::from_str(&text).map_err(TransportError::from))
            .transpose()
    }

    fn peer_label(&self) -> &str {
        &self.peer_label
    }
}

struct PrefixedTransport {
    inner: WebSocketTransport,
    first: Mutex<Option<ClientMessage>>,
}

impl PrefixedTransport {
    fn new(inner: WebSocketTransport, first: ClientMessage) -> Self {
        Self {
            inner,
            first: Mutex::new(Some(first)),
        }
    }
}

#[async_trait]
impl ClientTransport for PrefixedTransport {
    async fn send(&self, message: &ServerMessage) -> Result<(), TransportError> {
        self.inner.send(message).await
    }

    async fn recv(&self) -> Result<Option<ClientMessage>, TransportError> {
        if let Some(first) = self
            .first
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return Ok(Some(first));
        }
        self.inner.recv().await
    }

    fn peer_label(&self) -> &str {
        self.inner.peer_label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use futures_util::{SinkExt, StreamExt};

    #[test]
    fn browser_origins_require_explicit_allowlist_match() {
        let mut allowed = HashSet::new();
        allowed.insert("https://remote.example".to_string());
        let mut headers = HeaderMap::new();
        assert!(origin_allowed(&headers, &allowed));
        headers.insert(
            axum::http::header::ORIGIN,
            "https://remote.example".parse().unwrap(),
        );
        assert!(origin_allowed(&headers, &allowed));
        headers.insert(
            axum::http::header::ORIGIN,
            "https://evil.example".parse().unwrap(),
        );
        assert!(!origin_allowed(&headers, &allowed));
    }

    #[test]
    fn admission_limits_active_and_repeated_unauthenticated_peers() {
        let state = Arc::new(Mutex::new(AdmissionState::default()));
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let first = admit_peer(&state, peer, 3, 2, 10).unwrap();
        let second = admit_peer(&state, peer, 3, 2, 10).unwrap();
        assert_eq!(
            admit_peer(&state, peer, 3, 2, 10).err(),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        drop(first);
        let third = admit_peer(&state, peer, 3, 2, 10).unwrap();
        drop(second);
        drop(third);
        assert_eq!(
            admit_peer(&state, peer, 3, 2, 10).err(),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
    }

    #[test]
    fn forwarded_client_ip_requires_explicit_loopback_proxy_trust() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.4, 127.0.0.1".parse().unwrap());
        let loopback: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let public: SocketAddr = "192.0.2.10:1234".parse().unwrap();
        assert_eq!(
            admission_peer(&headers, loopback, false).unwrap(),
            loopback.ip()
        );
        assert_eq!(
            admission_peer(&headers, loopback, true).unwrap(),
            "203.0.113.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            admission_peer(&headers, public, true),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn admission_peer_map_is_bounded_and_evicts_idle_windows() {
        let state = Arc::new(Mutex::new(AdmissionState::default()));
        let first_peer: IpAddr = "203.0.113.1".parse().unwrap();
        let second_peer: IpAddr = "203.0.113.2".parse().unwrap();
        let first = admit_peer(&state, first_peer, 10, 1, 1).unwrap();
        assert_eq!(
            admit_peer(&state, second_peer, 10, 1, 1).err(),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        drop(first);
        state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .peers
            .get_mut(&first_peer)
            .unwrap()
            .window_started = std::time::Instant::now() - Duration::from_secs(61);
        assert!(admit_peer(&state, second_peer, 10, 1, 1).is_ok());
        assert_eq!(
            state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .peers
                .len(),
            1
        );
    }

    #[test]
    fn remote_labels_cannot_emit_terminal_controls_or_bidi_overrides() {
        assert_eq!(
            sanitize_remote_label("\u{1b}]0;owned\u{7}Phone\u{202e}"),
            "]0;ownedPhone"
        );
        assert_eq!(sanitize_remote_label("\n\t"), "Remote device");
    }

    #[test]
    fn message_rate_counts_control_frames_through_shared_budget() {
        let now = std::time::Instant::now();
        let mut rate = MessageRate {
            window_started: now,
            count: 0,
            max_per_second: 2,
        };
        assert!(rate.check(now).is_ok());
        assert!(rate.check(now).is_ok());
        assert!(matches!(rate.check(now), Err(TransportError::Backpressure)));
        assert!(rate.check(now + Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn authenticated_remote_identity_cannot_be_client_supplied() {
        let message = ClientMessage::Attach {
            protocol_version: 2,
            session_id: Some("session".to_string()),
            ticket: Some("leaked".to_string()),
            client: ClientInfo {
                id: "attacker".to_string(),
                kind: ClientKind::Browser,
                label: "attacker".to_string(),
            },
            requested_capabilities: vec![ClientCapability::Observe],
        };
        let bound = bind_remote_identity(message, "device", "phone").unwrap();
        assert!(matches!(
            bound,
            ClientMessage::Attach {
                ticket: None,
                client: ClientInfo { id, label, .. },
                ..
            } if id == "remote:device" && label == "phone"
        ));
    }

    #[test]
    fn only_one_distinct_remote_controller_holds_lease() {
        let state = Arc::new(Mutex::new(ControllerState::default()));
        let capabilities = vec![ClientCapability::Prompt];
        let first = acquire_controller(&state, "device-1", &capabilities)
            .unwrap()
            .unwrap();
        assert!(acquire_controller(&state, "device-2", &capabilities).is_err());
        assert!(acquire_controller(&state, "device-1", &capabilities).is_err());
        drop(first);
        assert!(acquire_controller(&state, "device-2", &capabilities).is_ok());
    }

    #[test]
    fn android_remote_auth_fixture_matches_gateway_envelopes() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../contracts/android/v2/remote_auth.json")).unwrap();
        for field in [
            "server_challenge",
            "pairing_pending",
            "pairing_approved",
            "pairing_committed",
            "authenticated",
        ] {
            serde_json::from_value::<RemoteServerFrame>(fixture[field].clone()).unwrap();
        }
        for field in ["pair_request", "authenticate_request"] {
            serde_json::from_value::<RemoteClientFrame>(fixture[field].clone()).unwrap();
        }
        let RemoteClientFrame::Authenticate {
            ticket,
            device_public_key,
            signature,
        } = serde_json::from_value(fixture["authenticate_request"].clone()).unwrap()
        else {
            panic!("fixture must contain authenticate request");
        };
        let public_key: [u8; 32] = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(device_public_key)
            .unwrap()
            .try_into()
            .unwrap();
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .unwrap();
        ed25519_dalek::VerifyingKey::from_bytes(&public_key)
            .unwrap()
            .verify_strict(
                &crate::remote_auth::auth_message("fixture-challenge", &ticket),
                &ed25519_dalek::Signature::from_slice(&signature).unwrap(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn websocket_pairing_and_authenticated_attach_are_end_to_end() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        let authenticator = Arc::new(PairingAuthority::default());
        let claim = authenticator.create_claim();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = RemoteGateway::new(
            daemon,
            Arc::clone(&authenticator),
            test_limits(),
            test_gateway_config(),
        );
        let server = tokio::spawn(gateway.serve(listener));
        let key = SigningKey::from_bytes(&[42; 32]);
        let encoded_key =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        let url = websocket_url(address);

        let (mut pairing_socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        assert!(matches!(
            recv_server_frame(&mut pairing_socket).await,
            RemoteServerFrame::Challenge { .. }
        ));
        pairing_socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&RemoteClientFrame::Pair {
                    claim: claim.secret,
                    device_public_key: encoded_key.clone(),
                    label: "test phone".to_string(),
                    requested_capabilities: vec![ClientCapability::Observe],
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let pending = match recv_server_frame(&mut pairing_socket).await {
            RemoteServerFrame::PairingPending { pairing } => pairing,
            other => panic!("expected pending pairing, got {other:?}"),
        };
        let grant = authenticator
            .approve(&pending.id, vec![ClientCapability::Observe])
            .unwrap();
        assert!(matches!(
            recv_server_frame(&mut pairing_socket).await,
            RemoteServerFrame::PairingApproved { .. }
        ));
        assert!(matches!(
            recv_server_frame(&mut pairing_socket).await,
            RemoteServerFrame::PairingCommitted { .. }
        ));
        pairing_socket.close(None).await.unwrap();

        let (mut remote_socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let challenge = match recv_server_frame(&mut remote_socket).await {
            RemoteServerFrame::Challenge { challenge } => challenge,
            other => panic!("expected challenge, got {other:?}"),
        };
        let signature = key.sign(&crate::remote_auth::auth_message(&challenge, &grant.ticket));
        remote_socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&RemoteClientFrame::Authenticate {
                    ticket: grant.ticket,
                    device_public_key: encoded_key,
                    signature: base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(signature.to_bytes()),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            recv_server_frame(&mut remote_socket).await,
            RemoteServerFrame::Authenticated { .. }
        ));
        remote_socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&ClientMessage::Attach {
                    protocol_version: crate::session_protocol::PROTOCOL_VERSION,
                    session_id: Some("missing".to_string()),
                    ticket: None,
                    client: ClientInfo {
                        id: "spoofed".to_string(),
                        kind: ClientKind::Android,
                        label: "spoofed".to_string(),
                    },
                    requested_capabilities: vec![ClientCapability::Observe],
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let response = remote_socket.next().await.unwrap().unwrap();
        let response: ServerMessage = serde_json::from_str(response.to_text().unwrap()).unwrap();
        assert!(matches!(response, ServerMessage::AttachDenied { .. }));

        server.abort();
    }

    #[tokio::test]
    async fn revocation_closes_authenticated_socket_before_attach() {
        let daemon = Arc::new(SessionDaemon::new(1, 1, 8, 32));
        let authenticator = Arc::new(PairingAuthority::default());
        let key = SigningKey::from_bytes(&[43; 32]);
        let encoded_key =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        let claim = authenticator.create_claim();
        let pending = authenticator
            .submit_pairing(
                &claim.secret,
                &encoded_key,
                "phone".to_string(),
                vec![ClientCapability::Observe],
            )
            .unwrap();
        let grant = authenticator
            .approve(&pending.id, vec![ClientCapability::Observe])
            .unwrap();
        authenticator.finish_pairing(&pending.id);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = RemoteGateway::new(
            daemon,
            Arc::clone(&authenticator),
            test_limits(),
            test_gateway_config(),
        );
        let server = tokio::spawn(gateway.serve(listener));
        let (mut socket, _) = tokio_tungstenite::connect_async(websocket_url(address))
            .await
            .unwrap();
        let challenge = match recv_server_frame(&mut socket).await {
            RemoteServerFrame::Challenge { challenge } => challenge,
            other => panic!("expected challenge, got {other:?}"),
        };
        let signature = key.sign(&crate::remote_auth::auth_message(&challenge, &grant.ticket));
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&RemoteClientFrame::Authenticate {
                    ticket: grant.ticket,
                    device_public_key: encoded_key,
                    signature: base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(signature.to_bytes()),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            recv_server_frame(&mut socket).await,
            RemoteServerFrame::Authenticated { .. }
        ));

        assert!(authenticator.revoke_device(&grant.device_id));
        let closed = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("revoked socket must close promptly");
        assert!(matches!(
            closed,
            None | Some(Err(_)) | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
        ));
        server.abort();
    }

    #[tokio::test]
    async fn disconnected_pairing_socket_releases_pending_request() {
        let authenticator = Arc::new(PairingAuthority::default());
        let claim = authenticator.create_claim();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = RemoteGateway::new(
            Arc::new(SessionDaemon::new(1, 1, 8, 32)),
            Arc::clone(&authenticator),
            test_limits(),
            test_gateway_config(),
        );
        let server = tokio::spawn(gateway.serve(listener));
        let key = SigningKey::from_bytes(&[44; 32]);
        let encoded_key =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        let (mut socket, _) = tokio_tungstenite::connect_async(websocket_url(address))
            .await
            .unwrap();
        assert!(matches!(
            recv_server_frame(&mut socket).await,
            RemoteServerFrame::Challenge { .. }
        ));
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&RemoteClientFrame::Pair {
                    claim: claim.secret,
                    device_public_key: encoded_key,
                    label: "phone".to_string(),
                    requested_capabilities: vec![ClientCapability::Observe],
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            recv_server_frame(&mut socket).await,
            RemoteServerFrame::PairingPending { .. }
        ));
        socket.close(None).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if authenticator.pending_pairings().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("disconnected pairing must be removed");
        server.abort();
    }

    fn test_limits() -> ProtocolLimits {
        ProtocolLimits {
            max_frame_bytes: 64 * 1024,
            max_prompt_bytes: 16 * 1024,
            max_label_bytes: 256,
            max_identifier_bytes: 256,
            max_cursor_bytes: 256,
            max_ticket_bytes: 512,
            max_runtime_value_bytes: 1024,
            max_capabilities: 16,
        }
    }

    fn test_gateway_config() -> RemoteGatewayConfig {
        RemoteGatewayConfig {
            allowed_origins: HashSet::new(),
            max_frame_bytes: 64 * 1024,
            pairing_wait: Duration::from_secs(2),
            auth_timeout: Duration::from_secs(2),
            heartbeat_interval: Duration::from_secs(30),
            heartbeat_timeout: Duration::from_secs(2),
            max_messages_per_second: 30,
            max_connections: 2,
            admission_attempts_per_minute: 6,
            max_unauthenticated_per_ip: 2,
            trust_proxy_headers: false,
            max_admission_peers: 128,
        }
    }

    fn websocket_url(address: SocketAddr) -> String {
        format!("{}://{address}/v2/ws", ["w", "s"].concat())
    }

    async fn recv_server_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> RemoteServerFrame {
        let message = socket.next().await.unwrap().unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }
}
