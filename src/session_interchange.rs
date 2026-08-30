//! Provider-neutral session import/export codecs (Vikunja #1410).

use serde::{Deserialize, Serialize};

use crate::cli::{SessionArgs, SessionCommand};
use crate::session_store::PersistedSession;

const DAIMONOS_SESSION_FORMAT: &str = "daimonos.session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFormat {
    Json,
}

impl SessionFormat {
    pub fn parse(value: &str) -> Result<Self, SessionInterchangeError> {
        match value.to_ascii_lowercase().as_str() {
            "json" => Ok(Self::Json),
            _ => Err(SessionInterchangeError::UnsupportedFormat(
                value.to_string(),
            )),
        }
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self, SessionInterchangeError> {
        path.extension()
            .and_then(|extension| extension.to_str())
            .ok_or_else(|| SessionInterchangeError::UnsupportedFormat(path.display().to_string()))
            .and_then(Self::parse)
    }
}

#[derive(Debug)]
pub enum SessionInterchangeError {
    UnsupportedFormat(String),
    InvalidEnvelope(String),
    Json(serde_json::Error),
}

impl std::fmt::Display for SessionInterchangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(format) => {
                write!(formatter, "unsupported session format: {format}")
            }
            Self::InvalidEnvelope(message) => {
                write!(formatter, "invalid session archive: {message}")
            }
            Self::Json(error) => write!(formatter, "invalid session JSON: {error}"),
        }
    }
}

impl std::error::Error for SessionInterchangeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::UnsupportedFormat(_) | Self::InvalidEnvelope(_) => None,
        }
    }
}

trait SessionCodec {
    fn encode(&self, session: &PersistedSession) -> Result<Vec<u8>, SessionInterchangeError>;
    fn decode(&self, bytes: &[u8]) -> Result<PersistedSession, SessionInterchangeError>;
}

struct JsonSessionCodec;

#[derive(Serialize, Deserialize)]
struct JsonSessionEnvelope {
    format: String,
    #[serde(flatten)]
    session: PersistedSession,
}

impl SessionCodec for JsonSessionCodec {
    fn encode(&self, session: &PersistedSession) -> Result<Vec<u8>, SessionInterchangeError> {
        let envelope = JsonSessionEnvelope {
            format: DAIMONOS_SESSION_FORMAT.to_string(),
            session: session.clone(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&envelope).map_err(SessionInterchangeError::Json)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn decode(&self, bytes: &[u8]) -> Result<PersistedSession, SessionInterchangeError> {
        let envelope: JsonSessionEnvelope =
            serde_json::from_slice(bytes).map_err(SessionInterchangeError::Json)?;
        if envelope.format != DAIMONOS_SESSION_FORMAT {
            return Err(SessionInterchangeError::InvalidEnvelope(format!(
                "expected format {DAIMONOS_SESSION_FORMAT}"
            )));
        }
        if envelope.session.session_id.is_empty() {
            return Err(SessionInterchangeError::InvalidEnvelope(
                "session_id is required".to_string(),
            ));
        }
        Ok(envelope.session)
    }
}

fn codec(format: SessionFormat) -> &'static dyn SessionCodec {
    match format {
        SessionFormat::Json => &JsonSessionCodec,
    }
}

pub fn encode(
    format: SessionFormat,
    session: &PersistedSession,
) -> Result<Vec<u8>, SessionInterchangeError> {
    codec(format).encode(session)
}

pub fn decode(
    format: SessionFormat,
    bytes: &[u8],
) -> Result<PersistedSession, SessionInterchangeError> {
    codec(format).decode(bytes)
}

pub fn run(args: SessionArgs, config: &crate::config::Config) -> anyhow::Result<()> {
    let directory = crate::paths::daemon_sessions_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve session database directory"))?;
    let store = crate::session_store::SessionStore::new(directory).with_busy_timeout(
        std::time::Duration::from_millis(config.session.session_store_busy_timeout_ms),
    );
    match args.command {
        SessionCommand::Import { file } => {
            let format = SessionFormat::from_path(&file)?;
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(&file)?
                .take(config.session.session_archive_max_bytes as u64 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= config.session.session_archive_max_bytes,
                "session archive exceeds configured byte limit"
            );
            let session = decode(format, &bytes)?;
            let session_id = session.session_id.clone();
            store
                .import_if_absent(session, config.session.session_list_preview_bytes)
                .map_err(|error| anyhow::anyhow!("session import failed: {error}"))?;
            println!("{session_id}");
        }
        SessionCommand::Export {
            session_id,
            format,
            output,
        } => {
            let format = SessionFormat::parse(&format)?;
            let session = store
                .load_result(&session_id)
                .map_err(|error| anyhow::anyhow!("session export failed: {error}"))?;
            let bytes = encode(format, &session)?;
            match output {
                Some(path) => {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                }
                None => {
                    use std::io::Write;
                    std::io::stdout().write_all(&bytes)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> PersistedSession {
        PersistedSession {
            version: crate::session_store::SESSION_PERSIST_VERSION,
            generation: 7,
            session_id: "session-1".to_string(),
            model: "test-model".to_string(),
            thinking: Some("medium".to_string()),
            cwd: Some(std::path::PathBuf::from("/workspace")),
            client_user_message_ids: vec!["user-1".to_string()],
            assistant_outcomes: Vec::new(),
            messages: vec![crate::providers::Message::user("hello")],
        }
    }

    #[test]
    fn json_archive_round_trips_provider_neutral_session() {
        let bytes = encode(SessionFormat::Json, &session()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["format"], DAIMONOS_SESSION_FORMAT);
        assert_eq!(json["session_id"], "session-1");

        let decoded = decode(SessionFormat::Json, &bytes).unwrap();
        assert_eq!(decoded.session_id, "session-1");
        assert_eq!(decoded.generation, 7);
        assert_eq!(decoded.messages.len(), 1);
    }

    #[test]
    fn json_archive_requires_daimonos_envelope_and_session_id() {
        let wrong = br#"{"format":"other","version":1,"session_id":"x","model":"m","messages":[]}"#;
        assert!(matches!(
            decode(SessionFormat::Json, wrong),
            Err(SessionInterchangeError::InvalidEnvelope(_))
        ));
        let missing =
            br#"{"format":"daimonos.session","version":1,"session_id":"","model":"m","messages":[]}"#;
        assert!(matches!(
            decode(SessionFormat::Json, missing),
            Err(SessionInterchangeError::InvalidEnvelope(_))
        ));
    }
}
