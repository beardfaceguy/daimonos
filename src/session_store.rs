//! SQLite persistence for agent conversations, shared by every agent frontend.
//! SQL stays private to this module so a future tamper-evident mutation ledger
//! can cover one stable transactional boundary (Vikunja #1410, #1411).

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::providers::{ContentBlock, Message, Role};
use crate::session_protocol::AssistantOutcome;

/// Version tag on the persisted-session JSON, so a future on-disk format
/// change can be detected and old files ignored rather than mis-parsed.
pub const SESSION_PERSIST_VERSION: u32 = 1;
const SESSION_STORE_SCHEMA_VERSION: i64 = 1;
const SESSION_DATABASE_NAME: &str = "sessions.sqlite3";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStoreErrorKind {
    UnsafeId,
    AlreadyExists,
    WriterChanged,
    NotFound,
    FutureVersion,
    UnsupportedVersion,
    Corrupt,
    Database,
    Permission,
    Io,
}

#[derive(Debug)]
pub enum SessionStoreError {
    UnsafeId,
    AlreadyExists,
    WriterChanged { expected: u64, found: Option<u64> },
    NotFound,
    FutureVersion { found: u32, supported: u32 },
    UnsupportedVersion { found: u32, supported: u32 },
    Corrupt(serde_json::Error),
    Database(rusqlite::Error),
    Io(std::io::Error),
}

impl SessionStoreError {
    pub fn kind(&self) -> SessionStoreErrorKind {
        match self {
            Self::UnsafeId => SessionStoreErrorKind::UnsafeId,
            Self::AlreadyExists => SessionStoreErrorKind::AlreadyExists,
            Self::WriterChanged { .. } => SessionStoreErrorKind::WriterChanged,
            Self::NotFound => SessionStoreErrorKind::NotFound,
            Self::FutureVersion { .. } => SessionStoreErrorKind::FutureVersion,
            Self::UnsupportedVersion { .. } => SessionStoreErrorKind::UnsupportedVersion,
            Self::Corrupt(_) => SessionStoreErrorKind::Corrupt,
            Self::Database(_) => SessionStoreErrorKind::Database,
            Self::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                SessionStoreErrorKind::Permission
            }
            Self::Io(_) => SessionStoreErrorKind::Io,
        }
    }
}

impl std::fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeId => formatter.write_str("session id is not a safe path component"),
            Self::AlreadyExists => formatter.write_str("session id already exists"),
            Self::WriterChanged { expected, found } => write!(
                formatter,
                "session changed while claiming writer epoch: expected generation {expected}, found {found:?}"
            ),
            Self::NotFound => formatter.write_str("persisted session was not found"),
            Self::FutureVersion { found, supported } => write!(
                formatter,
                "persisted session version {found} is newer than supported version {supported}"
            ),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "persisted session version {found} is older than supported version {supported}"
            ),
            Self::Corrupt(error) => write!(formatter, "persisted session JSON is invalid: {error}"),
            Self::Database(error) => write!(formatter, "session database failed: {error}"),
            Self::Io(error) => write!(formatter, "session store I/O failed: {error}"),
        }
    }
}

impl std::error::Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Corrupt(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::UnsafeId
            | Self::AlreadyExists
            | Self::WriterChanged { .. }
            | Self::NotFound
            | Self::FutureVersion { .. }
            | Self::UnsupportedVersion { .. } => None,
        }
    }
}

/// One session's on-disk record: enough to rebuild a conversation (history +
/// the model it was on).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub version: u32,
    #[serde(default)]
    pub generation: u64,
    pub session_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_user_message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assistant_outcomes: Vec<AssistantOutcome>,
    pub messages: Vec<Message>,
}

/// Lightweight metadata for listing saved sessions without loading every
/// message into the caller (though we do parse each file — histories are
/// small). `updated` is the file's mtime, used only for sort order.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub model: String,
    pub message_count: usize,
    pub cwd: Option<PathBuf>,
    pub updated_at: Option<std::time::SystemTime>,
    /// First line of the first user message, for a human-recognizable label.
    pub first_user_line: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PersistedWrite {
    pub summary: SessionSummary,
    pub updated_at_unix_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWriter {
    session_id: String,
    epoch: String,
}

#[derive(Debug, Clone)]
pub enum SessionWriteOutcome {
    Saved(PersistedWrite),
    Stale { stored_generation: u64 },
    Superseded,
}

#[derive(Debug, Clone)]
pub struct SessionSummaryScan {
    pub summaries: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

/// A directory of persisted sessions.
#[derive(Clone)]
pub struct SessionStore {
    dir: PathBuf,
    busy_timeout: Duration,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        SessionStore {
            dir,
            busy_timeout: Duration::ZERO,
        }
    }

    pub fn with_busy_timeout(mut self, busy_timeout: Duration) -> Self {
        self.busy_timeout = busy_timeout;
        self
    }

    fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    fn connection(&self) -> Result<Connection, SessionStoreError> {
        std::fs::create_dir_all(&self.dir).map_err(SessionStoreError::Io)?;
        let metadata = std::fs::metadata(&self.dir).map_err(SessionStoreError::Io)?;
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(SessionStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session store directory is not owned by the current user",
            )));
        }
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
            .map_err(SessionStoreError::Io)?;
        let path = self.dir.join(SESSION_DATABASE_NAME);
        let mut connection = Connection::open(&path).map_err(SessionStoreError::Database)?;
        connection
            .busy_timeout(self.busy_timeout)
            .map_err(SessionStoreError::Database)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(SessionStoreError::Database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(SessionStoreError::Database)?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_meta (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    version INTEGER NOT NULL
                 );
                 INSERT OR IGNORE INTO schema_meta (id, version) VALUES (1, 1);",
            )
            .map_err(SessionStoreError::Database)?;
        let schema_version = transaction
            .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(SessionStoreError::Database)?;
        if schema_version > SESSION_STORE_SCHEMA_VERSION {
            transaction
                .rollback()
                .map_err(SessionStoreError::Database)?;
            return Err(SessionStoreError::FutureVersion {
                found: schema_version as u32,
                supported: SESSION_STORE_SCHEMA_VERSION as u32,
            });
        }
        if schema_version < SESSION_STORE_SCHEMA_VERSION {
            transaction
                .rollback()
                .map_err(SessionStoreError::Database)?;
            return Err(SessionStoreError::UnsupportedVersion {
                found: schema_version.max(0) as u32,
                supported: SESSION_STORE_SCHEMA_VERSION as u32,
            });
        }
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    session_id TEXT PRIMARY KEY,
                    record_version INTEGER NOT NULL,
                    generation INTEGER NOT NULL,
                    payload BLOB NOT NULL,
                    model TEXT NOT NULL,
                    cwd TEXT,
                    message_count INTEGER NOT NULL,
                    preview TEXT,
                    updated_at_unix_ns INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS session_writers (
                    session_id TEXT PRIMARY KEY,
                    writer_epoch TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS sessions_updated
                    ON sessions(updated_at_unix_ns DESC, session_id ASC);",
            )
            .map_err(SessionStoreError::Database)?;
        transaction.commit().map_err(SessionStoreError::Database)?;
        for database_file in [
            path.clone(),
            path.with_extension("sqlite3-wal"),
            path.with_extension("sqlite3-shm"),
        ] {
            if database_file.exists() {
                std::fs::set_permissions(database_file, std::fs::Permissions::from_mode(0o600))
                    .map_err(SessionStoreError::Io)?;
            }
        }
        Ok(connection)
    }

    pub fn claim_writer(
        &self,
        session_id: &str,
        expected_generation: Option<u64>,
    ) -> Result<SessionWriter, SessionStoreError> {
        if !Self::valid_id(session_id) {
            return Err(SessionStoreError::UnsafeId);
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(SessionStoreError::Database)?;
        let found_generation = transaction
            .query_row(
                "SELECT generation FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(SessionStoreError::Database)?;
        match expected_generation {
            Some(expected) if found_generation != Some(expected) => {
                transaction
                    .rollback()
                    .map_err(SessionStoreError::Database)?;
                return Err(SessionStoreError::WriterChanged {
                    expected,
                    found: found_generation,
                });
            }
            None if found_generation.is_some() => {
                transaction
                    .rollback()
                    .map_err(SessionStoreError::Database)?;
                return Err(SessionStoreError::AlreadyExists);
            }
            _ => {}
        }
        let epoch = uuid::Uuid::new_v4().to_string();
        transaction
            .execute(
                "INSERT INTO session_writers (session_id, writer_epoch)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET
                    writer_epoch = excluded.writer_epoch",
                params![session_id, epoch],
            )
            .map_err(SessionStoreError::Database)?;
        transaction.commit().map_err(SessionStoreError::Database)?;
        Ok(SessionWriter {
            session_id: session_id.to_string(),
            epoch,
        })
    }

    /// Persist a single-writer chat session. Generation is assigned inside the
    /// transaction; callers must not concurrently edit the same session id.
    /// Best-effort failures are logged and never fail the caller's turn.
    pub fn save(&self, id: &str, model: &str, messages: &[Message]) {
        self.save_record(id, model, None, messages, None, &[], &[]);
    }

    /// Persist a session and the working directory needed by ACP session/list.
    /// The cwd is optional in the on-disk format so pre-existing records remain
    /// readable and the chat store can continue using [`Self::save`].
    #[cfg(test)]
    pub fn save_with_cwd(&self, id: &str, model: &str, messages: &[Message], cwd: &Path) {
        self.save_record(id, model, None, messages, Some(cwd.to_path_buf()), &[], &[]);
    }

    /// Persist an ACP/daemon session including its provider-neutral effort
    /// level so every caller makes runtime-state ownership explicit.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn save_acp(
        &self,
        id: &str,
        model: &str,
        thinking: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
    ) {
        if let Err(error) = self.save_acp_result(
            id,
            model,
            thinking,
            messages,
            cwd,
            client_user_message_ids,
            assistant_outcomes,
            usize::MAX,
        ) {
            eprintln!("session store: failed to persist session {id}: {error}");
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn save_acp_result(
        &self,
        id: &str,
        model: &str,
        thinking: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
        max_preview_bytes: usize,
    ) -> std::io::Result<PersistedWrite> {
        match self.save_record_result(
            id,
            model,
            Some(thinking.to_string()),
            messages,
            Some(cwd.to_path_buf()),
            client_user_message_ids,
            assistant_outcomes,
            max_preview_bytes,
            None,
            None,
        )? {
            SessionWriteOutcome::Saved(write) => Ok(write),
            SessionWriteOutcome::Stale { .. } => Err(std::io::Error::other(
                "automatic session generation was stale",
            )),
            SessionWriteOutcome::Superseded => Err(std::io::Error::other(
                "automatic session writer was superseded",
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_acp_generation_result(
        &self,
        writer: &SessionWriter,
        generation: u64,
        model: &str,
        thinking: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
        max_preview_bytes: usize,
    ) -> std::io::Result<SessionWriteOutcome> {
        self.save_record_result(
            &writer.session_id,
            model,
            Some(thinking.to_string()),
            messages,
            Some(cwd.to_path_buf()),
            client_user_message_ids,
            assistant_outcomes,
            max_preview_bytes,
            Some(generation),
            Some(writer),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn save_record(
        &self,
        id: &str,
        model: &str,
        thinking: Option<String>,
        messages: &[Message],
        cwd: Option<PathBuf>,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
    ) {
        if !Self::valid_id(id) {
            return;
        }
        let record = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            generation: 0,
            session_id: id.to_string(),
            model: model.to_string(),
            thinking,
            cwd,
            client_user_message_ids: client_user_message_ids.to_vec(),
            assistant_outcomes: assistant_outcomes.to_vec(),
            messages: messages.to_vec(),
        };
        if let Err(e) = self.write_record(record, usize::MAX, None, None) {
            eprintln!("session store: failed to persist session {id}: {e}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn save_record_result(
        &self,
        id: &str,
        model: &str,
        thinking: Option<String>,
        messages: &[Message],
        cwd: Option<PathBuf>,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
        max_preview_bytes: usize,
        generation: Option<u64>,
        writer: Option<&SessionWriter>,
    ) -> std::io::Result<SessionWriteOutcome> {
        if !Self::valid_id(id) {
            return Err(std::io::Error::other("session id is invalid"));
        }
        let record = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            generation: generation.unwrap_or_default(),
            session_id: id.to_string(),
            model: model.to_string(),
            thinking,
            cwd: cwd.clone(),
            client_user_message_ids: client_user_message_ids.to_vec(),
            assistant_outcomes: assistant_outcomes.to_vec(),
            messages: messages.to_vec(),
        };
        self.write_record(record, max_preview_bytes, generation, writer)
    }

    fn write_record(
        &self,
        mut record: PersistedSession,
        max_preview_bytes: usize,
        requested_generation: Option<u64>,
        writer: Option<&SessionWriter>,
    ) -> std::io::Result<SessionWriteOutcome> {
        let mut connection = self.connection().map_err(std::io::Error::other)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(std::io::Error::other)?;
        let active_epoch = transaction
            .query_row(
                "SELECT writer_epoch FROM session_writers WHERE session_id = ?1",
                [&record.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(std::io::Error::other)?;
        let epoch_matches = match writer {
            Some(writer) => {
                debug_assert_eq!(writer.session_id, record.session_id);
                active_epoch.as_deref() == Some(writer.epoch.as_str())
            }
            None => active_epoch.is_none(),
        };
        if !epoch_matches {
            transaction.rollback().map_err(std::io::Error::other)?;
            return Ok(SessionWriteOutcome::Superseded);
        }
        let stored_generation = transaction
            .query_row(
                "SELECT generation FROM sessions WHERE session_id = ?1",
                [&record.session_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(std::io::Error::other)?;
        let generation = requested_generation
            .unwrap_or_else(|| stored_generation.unwrap_or_default().saturating_add(1));
        if stored_generation.is_some_and(|stored| generation <= stored) {
            transaction.rollback().map_err(std::io::Error::other)?;
            return Ok(SessionWriteOutcome::Stale {
                stored_generation: stored_generation.unwrap_or_default(),
            });
        }
        record.generation = generation;
        let payload = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        let updated_at = std::time::SystemTime::now();
        let updated_at_unix_ns = system_time_unix_ns(updated_at);
        let preview = first_user_preview(&record.messages, max_preview_bytes);
        transaction
            .execute(
                "INSERT INTO sessions (
                    session_id, record_version, generation, payload, model, cwd,
                    message_count, preview, updated_at_unix_ns
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(session_id) DO UPDATE SET
                    record_version = excluded.record_version,
                    generation = excluded.generation,
                    payload = excluded.payload,
                    model = excluded.model,
                    cwd = excluded.cwd,
                    message_count = excluded.message_count,
                    preview = excluded.preview,
                    updated_at_unix_ns = excluded.updated_at_unix_ns",
                params![
                    record.session_id,
                    record.version,
                    record.generation,
                    payload,
                    record.model,
                    record.cwd.as_ref().map(|path| path.to_string_lossy()),
                    record.messages.len() as u64,
                    preview,
                    updated_at_unix_ns,
                ],
            )
            .map_err(std::io::Error::other)?;
        transaction.commit().map_err(std::io::Error::other)?;
        Ok(SessionWriteOutcome::Saved(PersistedWrite {
            summary: SessionSummary {
                id: record.session_id,
                model: record.model,
                message_count: record.messages.len(),
                cwd: record.cwd,
                updated_at: Some(updated_at),
                first_user_line: preview,
            },
            updated_at_unix_ns,
        }))
    }

    pub fn load_result(&self, id: &str) -> Result<PersistedSession, SessionStoreError> {
        if !Self::valid_id(id) {
            return Err(SessionStoreError::UnsafeId);
        }
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT payload, generation FROM sessions WHERE session_id = ?1",
                [id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()
            .map_err(SessionStoreError::Database)?
            .ok_or(SessionStoreError::NotFound)?;
        let mut record = decode_persisted_session(&row.0)?;
        if record.session_id != id {
            return Err(SessionStoreError::Database(rusqlite::Error::InvalidQuery));
        }
        record.generation = row.1;
        Ok(record)
    }

    /// Compatibility projection for chat/ACP callers that still treat every
    /// load failure as not resumable. Non-absence failures retain raw detail in
    /// structured logs; daemon loading uses [`Self::load_result`] directly.
    pub fn load(&self, id: &str) -> Option<PersistedSession> {
        match self.load_result(id) {
            Ok(record) => Some(record),
            Err(SessionStoreError::NotFound | SessionStoreError::UnsafeId) => None,
            Err(error) => {
                tracing::warn!(
                    target: "daimonos::session_store",
                    event = "session_load_failed",
                    error_kind = ?error.kind(),
                    error = %error,
                    "persisted session could not be loaded"
                );
                None
            }
        }
    }

    pub fn import_if_absent(
        &self,
        mut record: PersistedSession,
        max_preview_bytes: usize,
    ) -> Result<(), SessionStoreError> {
        validate_persisted_session(&record)?;
        if !Self::valid_id(&record.session_id) {
            return Err(SessionStoreError::UnsafeId);
        }
        // Import starts a new local persistence lineage. Archive generations
        // are descriptive and must not be trusted to pin future local writes.
        record.generation = 1;
        let payload = serde_json::to_vec(&record).map_err(SessionStoreError::Corrupt)?;
        let preview = first_user_preview(&record.messages, max_preview_bytes);
        let updated_at_unix_ns = system_time_unix_ns(std::time::SystemTime::now());
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(SessionStoreError::Database)?;
        let exists = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sessions WHERE session_id = ?1
                    UNION ALL
                    SELECT 1 FROM session_writers WHERE session_id = ?1
                 )",
                [&record.session_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(SessionStoreError::Database)?;
        if exists {
            return Err(SessionStoreError::AlreadyExists);
        }
        transaction
            .execute(
                "INSERT INTO sessions (
                    session_id, record_version, generation, payload, model, cwd,
                    message_count, preview, updated_at_unix_ns
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    record.session_id,
                    record.version,
                    record.generation,
                    payload,
                    record.model,
                    record.cwd.as_ref().map(|path| path.to_string_lossy()),
                    record.messages.len() as u64,
                    preview,
                    updated_at_unix_ns,
                ],
            )
            .map_err(SessionStoreError::Database)?;
        transaction.commit().map_err(SessionStoreError::Database)
    }

    /// Delete a persisted session. Missing and unsafe ids are treated as
    /// already deleted so ACP session/delete remains idempotent.
    /// Administrative deletion by id. This intentionally revokes any active
    /// writer epoch; live runtime deletion should use [`Self::delete_writer`].
    pub fn delete_unconditionally(&self, id: &str) -> std::io::Result<bool> {
        if !Self::valid_id(id) {
            return Ok(false);
        }
        let mut connection = self.connection().map_err(std::io::Error::other)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(std::io::Error::other)?;
        let revokes_writer = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM session_writers WHERE session_id = ?1
                 )",
                [id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(std::io::Error::other)?;
        if revokes_writer {
            tracing::warn!(
                target: "daimonos::session_store",
                event = "session_writer_epoch_revoked",
                session_id = id,
            );
        }
        let changed = transaction
            .execute("DELETE FROM sessions WHERE session_id = ?1", [id])
            .map_err(std::io::Error::other)?;
        transaction
            .execute("DELETE FROM session_writers WHERE session_id = ?1", [id])
            .map_err(std::io::Error::other)?;
        transaction.commit().map_err(std::io::Error::other)?;
        Ok(changed > 0)
    }

    pub fn delete_writer(&self, writer: &SessionWriter) -> std::io::Result<Option<bool>> {
        let mut connection = self.connection().map_err(std::io::Error::other)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(std::io::Error::other)?;
        let active_epoch = transaction
            .query_row(
                "SELECT writer_epoch FROM session_writers WHERE session_id = ?1",
                [&writer.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(std::io::Error::other)?;
        if active_epoch.as_deref() != Some(writer.epoch.as_str()) {
            transaction.rollback().map_err(std::io::Error::other)?;
            return Ok(None);
        }
        let changed = transaction
            .execute(
                "DELETE FROM sessions WHERE session_id = ?1",
                [&writer.session_id],
            )
            .map_err(std::io::Error::other)?;
        transaction
            .execute(
                "DELETE FROM session_writers WHERE session_id = ?1",
                [&writer.session_id],
            )
            .map_err(std::io::Error::other)?;
        transaction.commit().map_err(std::io::Error::other)?;
        Ok(Some(changed > 0))
    }

    #[cfg(test)]
    pub(crate) fn replace_payload_for_test(&self, id: &str, payload: &[u8]) -> std::io::Result<()> {
        let connection = self.connection().map_err(std::io::Error::other)?;
        connection
            .execute(
                "UPDATE sessions SET payload = ?1 WHERE session_id = ?2",
                params![payload, id],
            )
            .map(|_| ())
            .map_err(std::io::Error::other)
    }

    /// Summaries of all saved sessions, most-recently-modified first. Files
    /// that don't parse or fail a stat are skipped (best-effort listing).
    pub fn list(&self) -> Vec<SessionSummary> {
        self.list_with_preview_limit(usize::MAX)
    }

    /// Listing variant whose normalized first-user preview is UTF-8 byte
    /// bounded before it leaves the blocking store scan.
    pub fn list_with_preview_limit(&self, max_preview_bytes: usize) -> Vec<SessionSummary> {
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(error) => {
                log_store_error("session_list_failed", &error);
                return Vec::new();
            }
        };
        let mut statement = match connection.prepare(
            "SELECT session_id, model, message_count, cwd, preview, updated_at_unix_ns, payload
             FROM sessions ORDER BY updated_at_unix_ns DESC, session_id ASC",
        ) {
            Ok(statement) => statement,
            Err(error) => {
                log_store_error("session_list_failed", &SessionStoreError::Database(error));
                return Vec::new();
            }
        };
        let result = match statement.query_map([], |row| summary_from_row(row, max_preview_bytes)) {
            Ok(rows) => rows
                .filter_map(|row| match row {
                    Ok(summary) => Some(summary),
                    Err(error) => {
                        log_store_error(
                            "session_list_entry_failed",
                            &SessionStoreError::Database(error),
                        );
                        None
                    }
                })
                .collect(),
            Err(error) => {
                log_store_error("session_list_failed", &SessionStoreError::Database(error));
                Vec::new()
            }
        };
        result
    }

    /// Bounded stable-name batch for reconciliation and incomplete fallback.
    /// Directory walking stops at `deadline`; memory and payload parsing are
    /// capped by `max_entries`.
    pub fn scan_summaries(
        &self,
        max_preview_bytes: usize,
        after_name: Option<&str>,
        max_entries: usize,
        deadline: std::time::Instant,
    ) -> SessionSummaryScan {
        if std::time::Instant::now() >= deadline {
            return SessionSummaryScan {
                summaries: Vec::new(),
                next_cursor: after_name.map(str::to_string),
                complete: false,
            };
        }
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(error) => {
                log_store_error("session_scan_failed", &error);
                return SessionSummaryScan {
                    summaries: Vec::new(),
                    next_cursor: None,
                    complete: false,
                };
            }
        };
        let max_entries = max_entries.max(1);
        let after = after_name
            .and_then(|after| after.strip_suffix(".json").or(Some(after)))
            .unwrap_or_default();
        let limit = max_entries.saturating_add(1).min(i64::MAX as usize) as i64;
        let mut statement = match connection.prepare(
            "SELECT session_id, model, message_count, cwd, preview, updated_at_unix_ns, payload
             FROM sessions WHERE session_id > ?1 ORDER BY session_id ASC LIMIT ?2",
        ) {
            Ok(statement) => statement,
            Err(error) => {
                log_store_error("session_scan_failed", &SessionStoreError::Database(error));
                return SessionSummaryScan {
                    summaries: Vec::new(),
                    next_cursor: None,
                    complete: false,
                };
            }
        };
        let (mut rows, payload_complete) = match statement.query_map(params![after, limit], |row| {
            summary_from_row(row, max_preview_bytes)
        }) {
            Ok(rows) => {
                let mut complete = true;
                let rows = rows
                    .filter_map(|row| match row {
                        Ok(summary) => Some(summary),
                        Err(error) => {
                            complete = false;
                            log_store_error(
                                "session_scan_payload_failed",
                                &SessionStoreError::Database(error),
                            );
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                (rows, complete)
            }
            Err(error) => {
                log_store_error("session_scan_failed", &SessionStoreError::Database(error));
                return SessionSummaryScan {
                    summaries: Vec::new(),
                    next_cursor: None,
                    complete: false,
                };
            }
        };
        let has_more = rows.len() > max_entries;
        rows.truncate(max_entries);
        let next_cursor = has_more
            .then(|| rows.last().map(|summary| summary.id.clone()))
            .flatten();
        rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        SessionSummaryScan {
            summaries: rows,
            next_cursor,
            complete: !has_more && payload_complete && std::time::Instant::now() < deadline,
        }
    }
}

fn summary_from_row(
    row: &rusqlite::Row<'_>,
    max_preview_bytes: usize,
) -> rusqlite::Result<SessionSummary> {
    let updated_at_unix_ns = row.get::<_, u64>(5)?;
    let updated_at =
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(updated_at_unix_ns));
    let payload = row.get::<_, Vec<u8>>(6)?;
    let record = decode_persisted_session(&payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Blob, Box::new(error))
    })?;
    let preview = first_user_preview(&record.messages, max_preview_bytes);
    Ok(SessionSummary {
        id: record.session_id,
        model: record.model,
        message_count: record.messages.len(),
        cwd: record.cwd,
        updated_at,
        first_user_line: preview,
    })
}

fn decode_persisted_session(bytes: &[u8]) -> Result<PersistedSession, SessionStoreError> {
    let record: PersistedSession =
        serde_json::from_slice(bytes).map_err(SessionStoreError::Corrupt)?;
    validate_persisted_session(&record)?;
    Ok(record)
}

fn validate_persisted_session(record: &PersistedSession) -> Result<(), SessionStoreError> {
    if record.version > SESSION_PERSIST_VERSION {
        return Err(SessionStoreError::FutureVersion {
            found: record.version,
            supported: SESSION_PERSIST_VERSION,
        });
    }
    if record.version != SESSION_PERSIST_VERSION {
        return Err(SessionStoreError::UnsupportedVersion {
            found: record.version,
            supported: SESSION_PERSIST_VERSION,
        });
    }
    Ok(())
}

fn log_store_error(event: &'static str, error: &SessionStoreError) {
    tracing::warn!(
        target: "daimonos::session_store",
        event = event,
        error_kind = ?error.kind(),
        error = %error,
        "session store operation skipped an entry"
    );
}

fn system_time_unix_ns(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

/// Whitespace-normalized first line of the first user text message, bounded
/// without splitting a UTF-8 code point.
pub(crate) fn first_user_preview(messages: &[Message], max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text(text) => text
                    .lines()
                    .next()
                    .and_then(|line| normalize_preview(line, max_bytes)),
                _ => None,
            })
        })
}

pub(crate) fn normalize_preview(line: &str, max_bytes: usize) -> Option<String> {
    let mut preview = String::new();
    for word in line.split_whitespace() {
        let separator = usize::from(!preview.is_empty());
        let remaining = max_bytes.saturating_sub(preview.len());
        if remaining <= separator {
            break;
        }
        if separator == 1 {
            preview.push(' ');
        }
        let remaining = max_bytes.saturating_sub(preview.len());
        let boundary = crate::plugins::floor_char_boundary(word, remaining);
        preview.push_str(&word[..boundary]);
        if boundary < word.len() {
            break;
        }
    }
    (!preview.is_empty()).then_some(preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Message> {
        vec![
            Message::user("first question\nsecond line"),
            Message::assistant("the answer"),
        ]
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("abc-123", "test-model", &msgs());

        let loaded = store.load("abc-123").expect("saved session should load");
        assert_eq!(loaded.session_id, "abc-123");
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.messages.len(), 2);
        assert!(
            matches!(&loaded.messages[0].content[0], ContentBlock::Text(t) if t == "first question\nsecond line")
        );
    }

    #[test]
    fn transactional_generation_rejects_late_stale_write() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let writer = store.claim_writer("ordered", None).unwrap();
        let saved = store
            .save_acp_generation_result(
                &writer,
                2,
                "new-model",
                "medium",
                &[Message::user("new")],
                workspace.path(),
                &[],
                &[],
                64,
            )
            .unwrap();
        assert!(matches!(saved, SessionWriteOutcome::Saved(_)));

        let stale = store
            .save_acp_generation_result(
                &writer,
                1,
                "old-model",
                "medium",
                &[Message::user("old")],
                workspace.path(),
                &[],
                &[],
                64,
            )
            .unwrap();

        assert!(matches!(
            stale,
            SessionWriteOutcome::Stale {
                stored_generation: 2
            }
        ));
        let loaded = store.load_result("ordered").unwrap();
        assert_eq!(loaded.generation, 2);
        assert_eq!(loaded.model, "new-model");
    }

    #[test]
    fn reopened_writer_epoch_rejects_old_save_and_delete() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().to_path_buf());
        let old_writer = store.claim_writer("epoch", None).unwrap();
        assert!(matches!(
            store
                .save_acp_generation_result(
                    &old_writer,
                    1,
                    "old",
                    "medium",
                    &[Message::user("old state")],
                    workspace.path(),
                    &[],
                    &[],
                    64,
                )
                .unwrap(),
            SessionWriteOutcome::Saved(_)
        ));
        let reopened = store.load_result("epoch").unwrap();
        let new_writer = store
            .claim_writer("epoch", Some(reopened.generation))
            .unwrap();

        assert!(matches!(
            store
                .save_acp_generation_result(
                    &old_writer,
                    99,
                    "stale",
                    "medium",
                    &[Message::user("stale state")],
                    workspace.path(),
                    &[],
                    &[],
                    64,
                )
                .unwrap(),
            SessionWriteOutcome::Superseded
        ));
        assert_eq!(store.delete_writer(&old_writer).unwrap(), None);
        assert!(matches!(
            store
                .save_acp_generation_result(
                    &new_writer,
                    2,
                    "new",
                    "medium",
                    &[Message::user("new state")],
                    workspace.path(),
                    &[],
                    &[],
                    64,
                )
                .unwrap(),
            SessionWriteOutcome::Saved(_)
        ));
        assert_eq!(store.load_result("epoch").unwrap().model, "new");
        assert_eq!(store.delete_writer(&new_writer).unwrap(), Some(true));
        assert!(matches!(
            store.load_result("epoch"),
            Err(SessionStoreError::NotFound)
        ));
    }

    #[test]
    fn writer_only_reservation_can_be_reclaimed_after_failed_creation() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().to_path_buf());
        let abandoned = store.claim_writer("unpersisted", None).unwrap();
        let replacement = store.claim_writer("unpersisted", None).unwrap();

        assert!(matches!(
            store
                .save_acp_generation_result(
                    &abandoned,
                    1,
                    "old",
                    "medium",
                    &[],
                    workspace.path(),
                    &[],
                    &[],
                    64,
                )
                .unwrap(),
            SessionWriteOutcome::Superseded
        ));
        assert!(matches!(
            store
                .save_acp_generation_result(
                    &replacement,
                    1,
                    "new",
                    "medium",
                    &[],
                    workspace.path(),
                    &[],
                    &[],
                    64,
                )
                .unwrap(),
            SessionWriteOutcome::Saved(_)
        ));
    }

    #[test]
    fn import_rejects_duplicate_without_replacing_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("duplicate", "original", &msgs());
        let replacement = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            generation: 99,
            session_id: "duplicate".to_string(),
            model: "replacement".to_string(),
            thinking: None,
            cwd: None,
            client_user_message_ids: Vec::new(),
            assistant_outcomes: Vec::new(),
            messages: Vec::new(),
        };

        assert_eq!(
            store.import_if_absent(replacement, 64).unwrap_err().kind(),
            SessionStoreErrorKind::AlreadyExists
        );
        assert_eq!(store.load_result("duplicate").unwrap().model, "original");
    }

    #[test]
    fn save_with_cwd_round_trips_and_lists_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save_with_cwd("acp-1", "test-model", &msgs(), workspace.path());

        let loaded = store.load("acp-1").expect("saved session should load");
        assert_eq!(loaded.cwd.as_deref(), Some(workspace.path()));

        let summary = store
            .list()
            .into_iter()
            .find(|summary| summary.id == "acp-1")
            .expect("saved session should be listed");
        assert_eq!(summary.cwd.as_deref(), Some(workspace.path()));
        assert!(summary.updated_at.is_some());
    }

    #[test]
    fn acp_client_user_message_ids_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save_acp(
            "acp-ids",
            "test-model",
            "medium",
            &msgs(),
            workspace.path(),
            &["user-1".to_string()],
            &[],
        );

        let loaded = store.load("acp-ids").expect("saved session should load");
        assert_eq!(loaded.client_user_message_ids, vec!["user-1"]);
    }

    #[test]
    fn record_without_cwd_remains_readable() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("without-cwd", "m", &[]);

        let loaded = store
            .load("without-cwd")
            .expect("record without cwd should load");
        assert_eq!(loaded.cwd, None);
        assert!(loaded.client_user_message_ids.is_empty());
        assert_eq!(store.list()[0].cwd, None);
    }

    #[test]
    fn load_unknown_id_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        assert!(store.load("never-saved").is_none());
        assert_eq!(
            store.load_result("never-saved").unwrap_err().kind(),
            SessionStoreErrorKind::NotFound
        );
    }

    #[test]
    fn unsafe_id_is_rejected_not_traversed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        // A traversal-ish id must never resolve to a file; save is a no-op and
        // load returns None rather than reading outside the store dir.
        store.save("../../etc/passwd", "m", &msgs());
        assert!(store.load("../../etc/passwd").is_none());
        assert_eq!(
            store.load_result("../../etc/passwd").unwrap_err().kind(),
            SessionStoreErrorKind::UnsafeId
        );
    }

    #[test]
    fn version_mismatch_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        // Hand-write a record with a future version.
        let json = serde_json::json!({
            "version": SESSION_PERSIST_VERSION + 1,
            "generation": 1,
            "session_id": "future",
            "model": "m",
            "messages": [],
        });
        store.save("future", "m", &[]);
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE sessions SET payload = ?1 WHERE session_id = 'future'",
                [serde_json::to_vec(&json).unwrap()],
            )
            .unwrap();
        assert!(
            store.load("future").is_none(),
            "an unrecognized version must not be loaded"
        );
        assert_eq!(
            store.load_result("future").unwrap_err().kind(),
            SessionStoreErrorKind::FutureVersion
        );
    }

    #[test]
    fn corrupt_and_io_failures_remain_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("corrupt", "m", &[]);
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE sessions SET payload = ?1 WHERE session_id = 'corrupt'",
                [b"{not-json".as_slice()],
            )
            .unwrap();
        assert_eq!(
            store.load_result("corrupt").unwrap_err().kind(),
            SessionStoreErrorKind::Corrupt
        );

        let not_directory = dir.path().join("not-a-directory");
        std::fs::write(&not_directory, b"x").unwrap();
        let invalid_store = SessionStore::new(not_directory);
        assert_eq!(
            invalid_store.load_result("session").unwrap_err().kind(),
            SessionStoreErrorKind::Io
        );
        assert_eq!(
            SessionStoreError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .kind(),
            SessionStoreErrorKind::Permission
        );
    }

    #[test]
    fn bounded_scan_marks_corrupt_payloads_incomplete_without_exposing_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("valid", "model", &msgs());
        store.save("corrupt", "model", &msgs());
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE sessions SET payload = ?1 WHERE session_id = 'corrupt'",
                [b"{not-json".as_slice()],
            )
            .unwrap();

        let scan = store.scan_summaries(
            64,
            None,
            10,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(!scan.complete);
        assert_eq!(scan.summaries.len(), 1);
        assert_eq!(scan.summaries[0].id, "valid");
    }

    #[test]
    fn list_returns_saved_sessions_with_labels() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("s1", "model-a", &msgs());
        store.save("s2", "model-b", &[Message::user("only one")]);

        let list = store.list();
        assert_eq!(list.len(), 2);
        let s1 = list.iter().find(|s| s.id == "s1").unwrap();
        assert_eq!(s1.model, "model-a");
        assert_eq!(s1.message_count, 2);
        // Label is the FIRST LINE of the first user message, trimmed.
        assert_eq!(s1.first_user_line.as_deref(), Some("first question"));
    }

    #[test]
    fn bounded_preview_normalizes_whitespace_without_splitting_utf8() {
        let preview =
            first_user_preview(&[Message::user("  alpha   🦀🦀🦀 beta\nignored")], 13).unwrap();
        assert_eq!(preview, "alpha 🦀");
        assert!(preview.len() <= 13);
    }

    #[test]
    fn list_breaks_equal_mtime_ties_by_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        for id in ["session-b", "session-a"] {
            store.save(id, "model", &[]);
        }
        store
            .connection()
            .unwrap()
            .execute("UPDATE sessions SET updated_at_unix_ns = 1000", [])
            .unwrap();

        assert_eq!(
            store
                .list()
                .into_iter()
                .map(|summary| summary.id)
                .collect::<Vec<_>>(),
            vec!["session-a", "session-b"]
        );
    }

    #[test]
    fn delete_is_idempotent_and_rejects_unsafe_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("s1", "m", &msgs());

        assert!(store.delete_unconditionally("s1").unwrap());
        assert!(store.load("s1").is_none());
        assert!(!store.delete_unconditionally("s1").unwrap());
        assert!(!store.delete_unconditionally("../../etc/passwd").unwrap());
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("s1", "m", &msgs());
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"))
            .collect();
        assert!(
            leftover.is_empty(),
            "temp file should have been renamed away"
        );
    }

    #[test]
    fn concurrent_atomic_writers_never_share_temporary_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().to_path_buf())
            .with_busy_timeout(std::time::Duration::from_secs(1));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers = (0..8)
            .map(|index| {
                let store = store.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.save(
                        "shared",
                        &format!("model-{index}"),
                        &[Message::user(format!("message-{index}"))],
                    );
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(store.load("shared").is_some());
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .all(|entry| entry.path().extension().and_then(|value| value.to_str()) != Some("tmp")));
    }

    #[test]
    fn sqlite_database_and_wal_sidecars_remain_private() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().to_path_buf());
        let connection = store.connection().unwrap();
        connection
            .execute(
                "INSERT INTO sessions (
                    session_id, record_version, generation, payload, model,
                    message_count, updated_at_unix_ns
                 ) VALUES ('private', 1, 1, X'7B7D', 'm', 0, 1)",
                [],
            )
            .unwrap();

        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for entry in std::fs::read_dir(directory.path()).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(SESSION_DATABASE_NAME)
            {
                assert_eq!(
                    entry.metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn bounded_summary_scan_advances_stable_filename_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().to_path_buf());
        for id in ["a", "b", "c"] {
            store.save(id, "model", &[Message::user(id)]);
        }
        let first = store.scan_summaries(
            64,
            None,
            2,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(!first.complete);
        assert_eq!(first.next_cursor.as_deref(), Some("b"));
        let mut ids = first
            .summaries
            .into_iter()
            .map(|summary| summary.id)
            .collect::<Vec<_>>();
        let second = store.scan_summaries(
            64,
            first.next_cursor.as_deref(),
            2,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(second.complete);
        ids.extend(second.summaries.into_iter().map(|summary| summary.id));
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn expired_summary_scan_is_explicitly_incomplete() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().to_path_buf());
        store.save("a", "model", &[Message::user("a")]);
        let scan = store.scan_summaries(64, None, 2, std::time::Instant::now());
        assert!(!scan.complete);
        assert!(scan.summaries.is_empty());
    }
}
