#![allow(dead_code)] // SessionDaemon consumes this seam in the next slice.

use async_trait::async_trait;
use std::fmt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::session_protocol::{ClientMessage, ServerMessage};

pub const READ_CHUNK_BYTES: usize = 4096;

#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    Json(serde_json::Error),
    FrameTooLarge { max_bytes: usize },
    UnexpectedEof,
    Closed,
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
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::FrameTooLarge { .. } | Self::UnexpectedEof | Self::Closed => None,
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
pub trait ClientTransport: Send {
    async fn send(&mut self, message: &ServerMessage) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError>;
    fn peer_label(&self) -> &str;
}

pub struct InMemoryTransport {
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    peer_label: String,
}

pub struct InMemoryClient {
    outbound: mpsc::Sender<ClientMessage>,
    inbound: mpsc::Receiver<ServerMessage>,
}

pub fn in_memory_transport_pair(
    capacity: usize,
    peer_label: impl Into<String>,
) -> (InMemoryTransport, InMemoryClient) {
    let capacity = capacity.max(1);
    let (client_tx, server_rx) = mpsc::channel(capacity);
    let (server_tx, client_rx) = mpsc::channel(capacity);
    (
        InMemoryTransport {
            inbound: server_rx,
            outbound: server_tx,
            peer_label: peer_label.into(),
        },
        InMemoryClient {
            outbound: client_tx,
            inbound: client_rx,
        },
    )
}

impl InMemoryClient {
    pub async fn send(&self, message: ClientMessage) -> Result<(), TransportError> {
        self.outbound
            .send(message)
            .await
            .map_err(|_| TransportError::Closed)
    }

    pub async fn recv(&mut self) -> Option<ServerMessage> {
        self.inbound.recv().await
    }
}

#[async_trait]
impl ClientTransport for InMemoryTransport {
    async fn send(&mut self, message: &ServerMessage) -> Result<(), TransportError> {
        self.outbound
            .send(message.clone())
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        Ok(self.inbound.recv().await)
    }

    fn peer_label(&self) -> &str {
        &self.peer_label
    }
}

pub struct UnixSocketTransport {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    read_buffer: Vec<u8>,
    peer_label: String,
    max_frame_bytes: usize,
}

impl UnixSocketTransport {
    pub fn new(stream: UnixStream, peer_label: impl Into<String>, max_frame_bytes: usize) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader,
            writer,
            read_buffer: Vec::new(),
            peer_label: peer_label.into(),
            max_frame_bytes: max_frame_bytes.max(1),
        }
    }

    pub fn buffered_bytes(&self) -> usize {
        self.read_buffer.len()
    }

    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            if let Some(newline) = self.read_buffer.iter().position(|byte| *byte == b'\n') {
                if newline > self.max_frame_bytes {
                    return Err(TransportError::FrameTooLarge {
                        max_bytes: self.max_frame_bytes,
                    });
                }
                let mut frame: Vec<u8> = self.read_buffer.drain(..=newline).collect();
                frame.pop();
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(Some(frame));
            }
            if self.read_buffer.len() > self.max_frame_bytes {
                return Err(TransportError::FrameTooLarge {
                    max_bytes: self.max_frame_bytes,
                });
            }

            let mut chunk = [0_u8; READ_CHUNK_BYTES];
            let read = self.reader.read(&mut chunk).await?;
            if read == 0 {
                return if self.read_buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(TransportError::UnexpectedEof)
                };
            }
            self.read_buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

#[async_trait]
impl ClientTransport for UnixSocketTransport {
    async fn send(&mut self, message: &ServerMessage) -> Result<(), TransportError> {
        let mut frame = serde_json::to_vec(message)?;
        if frame.len() > self.max_frame_bytes {
            return Err(TransportError::FrameTooLarge {
                max_bytes: self.max_frame_bytes,
            });
        }
        frame.push(b'\n');
        self.writer.write_all(&frame).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
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
        let (mut server, mut client) = in_memory_transport_pair(2, "headless");
        client.send(ClientMessage::Ping).await.unwrap();
        assert_eq!(server.recv().await.unwrap(), Some(ClientMessage::Ping));

        server.send(&ServerMessage::Pong).await.unwrap();
        assert_eq!(client.recv().await, Some(ServerMessage::Pong));
        assert_eq!(server.peer_label(), "headless");
    }

    #[tokio::test]
    async fn in_memory_transport_applies_bounded_backpressure() {
        let (mut server, client) = in_memory_transport_pair(1, "headless");
        client.send(ClientMessage::Ping).await.unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            client.send(ClientMessage::Detach),
        )
        .await
        .is_err());
        assert_eq!(server.recv().await.unwrap(), Some(ClientMessage::Ping));
    }

    #[tokio::test]
    async fn in_memory_disconnect_returns_none() {
        let (mut server, client) = in_memory_transport_pair(1, "headless");
        drop(client);
        assert_eq!(server.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn unix_transport_accepts_partial_frames() {
        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let mut server = UnixSocketTransport::new(server_stream, "local", 1024);
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
        let mut server = UnixSocketTransport::new(server_stream, "local", 1024);
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
        let mut server = UnixSocketTransport::new(server_stream, "local", 1024);
        client_stream.write_all(b"not-json\n").await.unwrap();
        assert!(matches!(server.recv().await, Err(TransportError::Json(_))));
    }

    #[tokio::test]
    async fn unix_transport_rejects_oversized_frame_without_unbounded_growth() {
        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let mut server = UnixSocketTransport::new(server_stream, "local", 16);
        client_stream
            .write_all(b"12345678901234567\n")
            .await
            .unwrap();
        assert!(matches!(
            server.recv().await,
            Err(TransportError::FrameTooLarge { max_bytes: 16 })
        ));
        assert!(server.buffered_bytes() <= 16 + READ_CHUNK_BYTES);
    }

    #[tokio::test]
    async fn unix_transport_distinguishes_clean_disconnect_from_partial_frame_eof() {
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let mut server = UnixSocketTransport::new(server_stream, "local", 1024);
        drop(client_stream);
        assert_eq!(server.recv().await.unwrap(), None);

        let (server_stream, mut client_stream) = UnixStream::pair().unwrap();
        let mut server = UnixSocketTransport::new(server_stream, "local", 1024);
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
}
