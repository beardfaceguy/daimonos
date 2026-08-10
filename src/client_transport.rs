#![allow(dead_code)] // SessionDaemon consumes this seam in the next slice.

use async_trait::async_trait;
use std::fmt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};

use crate::session_protocol::{ClientMessage, ServerMessage};

pub const READ_CHUNK_BYTES: usize = 4096;

#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    Json(serde_json::Error),
    FrameTooLarge { max_bytes: usize },
    UnexpectedEof,
    Closed,
    Backpressure,
    InvalidFrameLimit,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transport I/O error: {error}"),
            Self::Json(error) => write!(f, "invalid transport JSON: {error}"),
            Self::FrameTooLarge { max_bytes } => {
                write!(f, "transport frame exceeds {max_bytes} bytes")
            }
            Self::UnexpectedEof => write!(f, "transport closed in the middle of a frame"),
            Self::Closed => write!(f, "transport channel is closed"),
            Self::Backpressure => write!(f, "transport channel is full"),
            Self::InvalidFrameLimit => write!(f, "transport frame limit must be positive"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::FrameTooLarge { .. }
            | Self::UnexpectedEof
            | Self::Closed
            | Self::Backpressure
            | Self::InvalidFrameLimit => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for TransportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[async_trait]
pub trait ClientTransport: Send + Sync {
    async fn send(&self, message: &ServerMessage) -> Result<(), TransportError>;
    async fn recv(&self) -> Result<Option<ClientMessage>, TransportError>;
    fn peer_label(&self) -> &str;
}

#[async_trait]
pub trait FrontendTransport: Send {
    async fn send(&mut self, message: ClientMessage) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<ServerMessage>, TransportError>;
    fn peer_label(&self) -> &str;
}

pub struct InMemoryTransport {
    inbound: Mutex<mpsc::Receiver<ClientMessage>>,
    outbound: mpsc::Sender<ServerMessage>,
    peer_label: String,
}

pub struct InMemoryClient {
    outbound: mpsc::Sender<ClientMessage>,
    inbound: mpsc::Receiver<ServerMessage>,
    peer_label: String,
}

pub fn in_memory_transport_pair(
    capacity: usize,
    peer_label: impl Into<String>,
) -> (InMemoryTransport, InMemoryClient) {
    let capacity = capacity.max(1);
    let peer_label = peer_label.into();
    let (client_tx, server_rx) = mpsc::channel(capacity);
    let (server_tx, client_rx) = mpsc::channel(capacity);
    (
        InMemoryTransport {
            inbound: Mutex::new(server_rx),
            outbound: server_tx,
            peer_label: peer_label.clone(),
        },
        InMemoryClient {
            outbound: client_tx,
            inbound: client_rx,
            peer_label,
        },
    )
}

#[async_trait]
impl FrontendTransport for InMemoryClient {
    async fn send(&mut self, message: ClientMessage) -> Result<(), TransportError> {
        InMemoryClient::send(self, message).await
    }

    async fn recv(&mut self) -> Result<Option<ServerMessage>, TransportError> {
        Ok(InMemoryClient::recv(self).await)
    }

    fn peer_label(&self) -> &str {
        &self.peer_label
    }
}

impl InMemoryClient {
    pub async fn send(&self, message: ClientMessage) -> Result<(), TransportError> {
        self.outbound
            .send(message)
            .await
            .map_err(|_| TransportError::Closed)
    }

    pub fn try_send(&self, message: ClientMessage) -> Result<(), TransportError> {
        self.outbound
            .try_send(message)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TransportError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => TransportError::Closed,
            })
    }

    pub async fn recv(&mut self) -> Option<ServerMessage> {
        self.inbound.recv().await
    }
}

#[async_trait]
impl ClientTransport for InMemoryTransport {
    async fn send(&self, message: &ServerMessage) -> Result<(), TransportError> {
        self.outbound
            .send(message.clone())
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Result<Option<ClientMessage>, TransportError> {
        Ok(self.inbound.lock().await.recv().await)
    }

    fn peer_label(&self) -> &str {
        &self.peer_label
    }
}

struct UnixReadState {
    reader: OwnedReadHalf,
    buffer: Vec<u8>,
}

pub struct UnixSocketTransport {
    reader: Mutex<UnixReadState>,
    writer: Mutex<OwnedWriteHalf>,
    peer_label: String,
    max_frame_bytes: usize,
}

impl UnixSocketTransport {
    pub fn new(
        stream: UnixStream,
        peer_label: impl Into<String>,
        max_frame_bytes: usize,
    ) -> Result<Self, TransportError> {
        if max_frame_bytes == 0 {
            return Err(TransportError::InvalidFrameLimit);
        }
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: Mutex::new(UnixReadState {
                reader,
                buffer: Vec::new(),
            }),
            writer: Mutex::new(writer),
            peer_label: peer_label.into(),
            max_frame_bytes,
        })
    }

    pub async fn buffered_bytes(&self) -> usize {
        self.reader.lock().await.buffer.len()
    }

    async fn read_frame(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut state = self.reader.lock().await;
        loop {
            if let Some(newline) = state.buffer.iter().position(|byte| *byte == b'\n') {
                if newline > self.max_frame_bytes {
                    return Err(TransportError::FrameTooLarge {
                        max_bytes: self.max_frame_bytes,
                    });
                }
                let mut frame: Vec<u8> = state.buffer.drain(..=newline).collect();
                frame.pop();
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(Some(frame));
            }
            if state.buffer.len() > self.max_frame_bytes {
                return Err(TransportError::FrameTooLarge {
                    max_bytes: self.max_frame_bytes,
                });
            }

            // Read at most one byte beyond the configured limit. That byte is
            // enough to classify an oversized unterminated frame without ever
            // buffering a full I/O chunk beyond the advertised bound.
            let remaining = self
                .max_frame_bytes
                .saturating_add(1)
                .saturating_sub(state.buffer.len());
            let mut chunk = [0_u8; READ_CHUNK_BYTES];
            let read = state
                .reader
                .read(&mut chunk[..remaining.min(READ_CHUNK_BYTES)])
                .await?;
            if read == 0 {
                return if state.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(TransportError::UnexpectedEof)
                };
            }
            state.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

#[async_trait]
impl ClientTransport for UnixSocketTransport {
    async fn send(&self, message: &ServerMessage) -> Result<(), TransportError> {
        let mut frame = serde_json::to_vec(message)?;
        if frame.len() > self.max_frame_bytes {
            return Err(TransportError::FrameTooLarge {
                max_bytes: self.max_frame_bytes,
            });
        }
        frame.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&frame).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn recv(&self) -> Result<Option<ClientMessage>, TransportError> {
        let Some(frame) = self.read_frame().await? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&frame)?))
    }

    fn peer_label(&self) -> &str {
        &self.peer_label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_protocol::{ClientMessage, ServerMessage};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn in_memory_transport_round_trips_both_directions() {
        let (server, mut client) = in_memory_transport_pair(2, "headless");
        client.send(ClientMessage::Ping).await.unwrap();
        assert_eq!(server.recv().await.unwrap(), Some(ClientMessage::Ping));

        server.send(&ServerMessage::Pong).await.unwrap();
        assert_eq!(client.recv().await, Some(ServerMessage::Pong));
        assert_eq!(server.peer_label(), "headless");
    }

    #[tokio::test]
    async fn in_memory_transport_applies_bounded_backpressure() {
        let (server, client) = in_memory_transport_pair(1, "headless");
        client.send(ClientMessage::Ping).await.unwrap();
        assert!(matches!(
            client.try_send(ClientMessage::Detach),
            Err(TransportError::Backpressure)
        ));
        assert_eq!(server.recv().await.unwrap(), Some(ClientMessage::Ping));
    }

    #[tokio::test]
    async fn in_memory_disconnect_returns_none() {
        let (server, client) = in_memory_transport_pair(1, "headless");
        drop(client);
        assert_eq!(server.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn unix_transport_accepts_partial_frames() {
        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let server = UnixSocketTransport::new(server_stream, "local", 1024).unwrap();
        client_stream.write_all(b"{\"type\":\"pro").await.unwrap();
        client_stream
            .write_all(b"mpt\",\"request_id\":\"p1\",\"text\":\"hi\"}\n")
            .await
            .unwrap();

        assert_eq!(
            server.recv().await.unwrap(),
            Some(ClientMessage::Prompt {
                request_id: "p1".to_string(),
                text: "hi".to_string(),
            })
        );
        assert_eq!(server.peer_label(), "local");
    }

    #[tokio::test]
    async fn unix_transport_sends_newline_delimited_json() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = UnixSocketTransport::new(server_stream, "local", 1024).unwrap();
        let mut client = BufReader::new(client_stream);

        server.send(&ServerMessage::Pong).await.unwrap();
        let mut line = String::new();
        client.read_line(&mut line).await.unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<ServerMessage>(line.trim_end()).unwrap(),
            ServerMessage::Pong
        );
    }

    #[tokio::test]
    async fn unix_transport_rejects_malformed_json() {
        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let server = UnixSocketTransport::new(server_stream, "local", 1024).unwrap();
        client_stream.write_all(b"not-json\n").await.unwrap();
        assert!(matches!(server.recv().await, Err(TransportError::Json(_))));
    }

    #[tokio::test]
    async fn unix_transport_rejects_oversized_frame_without_unbounded_growth() {
        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let server = UnixSocketTransport::new(server_stream, "local", 16).unwrap();
        client_stream
            .write_all(b"12345678901234567\n")
            .await
            .unwrap();
        assert!(matches!(
            server.recv().await,
            Err(TransportError::FrameTooLarge { max_bytes: 16 })
        ));
        assert!(server.buffered_bytes().await <= 17);
    }

    #[tokio::test]
    async fn unix_transport_distinguishes_clean_disconnect_from_partial_frame_eof() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let server = UnixSocketTransport::new(server_stream, "local", 1024).unwrap();
        drop(client_stream);
        assert_eq!(server.recv().await.unwrap(), None);

        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let server = UnixSocketTransport::new(server_stream, "local", 1024).unwrap();
        client_stream
            .write_all(b"{\"type\":\"ping\"}")
            .await
            .unwrap();
        client_stream.shutdown().await.unwrap();
        assert!(matches!(
            server.recv().await,
            Err(TransportError::UnexpectedEof)
        ));
    }

    #[tokio::test]
    async fn unix_transport_rejects_zero_frame_limit() {
        let (server_stream, _client_stream) = UnixStream::pair().unwrap();
        assert!(matches!(
            UnixSocketTransport::new(server_stream, "local", 0),
            Err(TransportError::InvalidFrameLimit)
        ));
    }

    #[tokio::test]
    async fn unix_transport_can_send_while_receive_is_pending() {
        use std::sync::Arc;

        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let server = Arc::new(UnixSocketTransport::new(server_stream, "local", 1024).unwrap());
        let receiver = Arc::clone(&server);
        let recv_task = tokio::spawn(async move { receiver.recv().await });

        server.send(&ServerMessage::Pong).await.unwrap();
        let mut client = BufReader::new(&mut client_stream);
        let mut line = String::new();
        client.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(line.trim_end()).unwrap(),
            ServerMessage::Pong
        );
        drop(client);

        client_stream
            .write_all(b"{\"type\":\"ping\"}\n")
            .await
            .unwrap();
        assert_eq!(recv_task.await.unwrap().unwrap(), Some(ClientMessage::Ping));
    }
}
