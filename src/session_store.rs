//! On-disk persistence for agent conversations, shared by the ACP engine
//! (`src/acp_cmd.rs`, vikunja #961) and the chat REPL (`src/chat_cmd.rs`,
//! vikunja #963). One versioned JSON file per session, keyed by a plain
//! string id, written atomically (temp + rename) so a crash mid-write can't
//! leave a truncated file. Mirrors how Zed's native providers restore full
//! history from a local store.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::providers::{ContentBlock, Message, Role};
use crate::session_protocol::AssistantOutcome;

/// Version tag on the persisted-session JSON, so a future on-disk format
/// change can be detected and old files ignored rather than mis-parsed.
pub const SESSION_PERSIST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStoreErrorKind {
    UnsafeId,
    NotFound,
    FutureVersion,
    UnsupportedVersion,
    Corrupt,
    Permission,
    Io,
}

#[derive(Debug)]
pub enum SessionStoreError {
    UnsafeId,
    NotFound,
    FutureVersion { found: u32, supported: u32 },
    UnsupportedVersion { found: u32, supported: u32 },
    Corrupt(serde_json::Error),
    Io(std::io::Error),
}

impl SessionStoreError {
    pub fn kind(&self) -> SessionStoreErrorKind {
        match self {
            Self::UnsafeId => SessionStoreErrorKind::UnsafeId,
            Self::NotFound => SessionStoreErrorKind::NotFound,
            Self::FutureVersion { .. } => SessionStoreErrorKind::FutureVersion,
            Self::UnsupportedVersion { .. } => SessionStoreErrorKind::UnsupportedVersion,
            Self::Corrupt(_) => SessionStoreErrorKind::Corrupt,
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
            Self::Io(error) => write!(formatter, "session store I/O failed: {error}"),
        }
    }
}

impl std::error::Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Corrupt(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::UnsafeId
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
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        SessionStore { dir }
    }

    /// Filename for a session id, or `None` if the id has characters unsafe as
    /// a path component. Minted ids are UUIDs; this only guards a hostile or
    /// malformed id against path traversal / collisions.
    fn file_name(id: &str) -> Option<String> {
        let safe = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        safe.then(|| format!("{id}.json"))
    }

    /// Persist a session's history. Best-effort: a write failure is logged to
    /// stderr and never fails the caller's turn.
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
        self.save_record_result(
            id,
            model,
            Some(thinking.to_string()),
            messages,
            Some(cwd.to_path_buf()),
            client_user_message_ids,
            assistant_outcomes,
            max_preview_bytes,
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
        let Some(name) = Self::file_name(id) else {
            return;
        };
        let record = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            session_id: id.to_string(),
            model: model.to_string(),
            thinking,
            cwd,
            client_user_message_ids: client_user_message_ids.to_vec(),
            assistant_outcomes: assistant_outcomes.to_vec(),
            messages: messages.to_vec(),
        };
        if let Err(e) = self.write_atomic(&name, &record) {
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
    ) -> std::io::Result<PersistedWrite> {
        let name = Self::file_name(id)
            .ok_or_else(|| std::io::Error::other("session id is not a safe path component"))?;
        let record = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            session_id: id.to_string(),
            model: model.to_string(),
            thinking,
            cwd: cwd.clone(),
            client_user_message_ids: client_user_message_ids.to_vec(),
            assistant_outcomes: assistant_outcomes.to_vec(),
            messages: messages.to_vec(),
        };
        let updated_at = self.write_atomic(&name, &record)?;
        Ok(PersistedWrite {
            summary: SessionSummary {
                id: id.to_string(),
                model: model.to_string(),
                message_count: messages.len(),
                cwd,
                updated_at: Some(updated_at),
                first_user_line: first_user_preview(messages, max_preview_bytes),
            },
            updated_at_unix_ns: system_time_unix_ns(updated_at),
        })
    }

    fn write_atomic(
        &self,
        name: &str,
        record: &PersistedSession,
    ) -> std::io::Result<std::time::SystemTime> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        // Write to a temp file then rename, so a crash mid-write can't leave a
        // truncated JSON file that would fail to load.
        let tmp = self
            .dir
            .join(format!("{name}.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, &json)?;
        let path = self.dir.join(name);
        if let Err(error) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        std::fs::metadata(path)?.modified()
    }

    pub fn load_result(&self, id: &str) -> Result<PersistedSession, SessionStoreError> {
        let name = Self::file_name(id).ok_or(SessionStoreError::UnsafeId)?;
        let bytes = std::fs::read(self.dir.join(name)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SessionStoreError::NotFound
            } else {
                SessionStoreError::Io(error)
            }
        })?;
        decode_persisted_session(&bytes)
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

    /// Delete a persisted session. Missing and unsafe ids are treated as
    /// already deleted so ACP session/delete remains idempotent.
    pub fn delete(&self, id: &str) -> std::io::Result<bool> {
        let Some(name) = Self::file_name(id) else {
            return Ok(false);
        };
        match std::fs::remove_file(self.dir.join(name)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Summaries of all saved sessions, most-recently-modified first. Files
    /// that don't parse or fail a stat are skipped (best-effort listing).
    pub fn list(&self) -> Vec<SessionSummary> {
        self.list_with_preview_limit(usize::MAX)
    }

    /// Listing variant whose normalized first-user preview is UTF-8 byte
    /// bounded before it leaves the blocking store scan.
    pub fn list_with_preview_limit(&self, max_preview_bytes: usize) -> Vec<SessionSummary> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                log_store_error("session_list_failed", &SessionStoreError::Io(error));
                return Vec::new();
            }
        };
        let mut rows: Vec<(std::time::SystemTime, SessionSummary)> = Vec::new();
        for entry in entries {
            match entry
                .map_err(SessionStoreError::Io)
                .and_then(|entry| summary_from_path(entry.path(), max_preview_bytes))
            {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                Err(error) => log_store_error("session_list_entry_failed", &error),
            }
        }
        rows.sort_by(|(a_time, a_summary), (b_time, b_summary)| {
            b_time
                .cmp(a_time)
                .then_with(|| a_summary.id.cmp(&b_summary.id))
        });
        rows.into_iter().map(|(_, s)| s).collect()
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
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return SessionSummaryScan {
                    summaries: Vec::new(),
                    next_cursor: None,
                    complete: true,
                };
            }
            Err(error) => {
                log_store_error("session_scan_failed", &SessionStoreError::Io(error));
                return SessionSummaryScan {
                    summaries: Vec::new(),
                    next_cursor: None,
                    complete: false,
                };
            }
        };
        let max_entries = max_entries.max(1);
        let mut names = std::collections::BTreeMap::new();
        let mut walk_complete = true;
        for entry in entries {
            if std::time::Instant::now() >= deadline {
                walk_complete = false;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    walk_complete = false;
                    log_store_error("session_scan_entry_failed", &SessionStoreError::Io(error));
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name_text) = name.to_str() else {
                continue;
            };
            if !name_text.ends_with(".json") || after_name.is_some_and(|after| name_text <= after) {
                continue;
            }
            names.insert(name_text.to_string(), entry.path());
            if names.len() > max_entries.saturating_add(1) {
                names.pop_last();
            }
        }
        let has_more = !walk_complete || names.len() > max_entries;
        while names.len() > max_entries {
            names.pop_last();
        }
        let next_cursor = has_more
            .then(|| names.last_key_value().map(|(name, _)| name.clone()))
            .flatten();
        let mut payload_complete = true;
        let mut rows = Vec::new();
        for path in names.into_values() {
            match summary_from_path(path, max_preview_bytes) {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                Err(error) => {
                    payload_complete = false;
                    log_store_error("session_scan_payload_failed", &error);
                }
            }
        }
        rows.sort_by(|(a_time, a_summary), (b_time, b_summary)| {
            b_time
                .cmp(a_time)
                .then_with(|| a_summary.id.cmp(&b_summary.id))
        });
        SessionSummaryScan {
            summaries: rows.into_iter().map(|(_, summary)| summary).collect(),
            next_cursor,
            complete: !has_more && payload_complete,
        }
    }
}

fn summary_from_path(
    path: PathBuf,
    max_preview_bytes: usize,
) -> Result<Option<(std::time::SystemTime, SessionSummary)>, SessionStoreError> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(SessionStoreError::Io)?;
    let record = decode_persisted_session(&bytes)?;
    let modified = match std::fs::metadata(&path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(error) => {
            log_store_error(
                "session_summary_timestamp_failed",
                &SessionStoreError::Io(error),
            );
            std::time::UNIX_EPOCH
        }
    };
    Ok(Some((
        modified,
        SessionSummary {
            id: record.session_id,
            model: record.model,
            message_count: record.messages.len(),
            cwd: record.cwd,
            updated_at: Some(modified),
            first_user_line: first_user_preview(&record.messages, max_preview_bytes),
        },
    )))
}

fn decode_persisted_session(bytes: &[u8]) -> Result<PersistedSession, SessionStoreError> {
    let record: PersistedSession =
        serde_json::from_slice(bytes).map_err(SessionStoreError::Corrupt)?;
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
    Ok(record)
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
    fn legacy_record_without_cwd_remains_readable() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let json = serde_json::json!({
            "version": SESSION_PERSIST_VERSION,
            "session_id": "legacy",
            "model": "m",
            "messages": [],
        });
        std::fs::write(
            dir.path().join("legacy.json"),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();

        let loaded = store.load("legacy").expect("legacy record should load");
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
            "session_id": "future",
            "model": "m",
            "messages": [],
        });
        std::fs::write(
            dir.path().join("future.json"),
            serde_json::to_vec(&json).unwrap(),
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
        std::fs::write(dir.path().join("corrupt.json"), b"{not-json").unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
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
        std::fs::write(dir.path().join("corrupt.json"), b"{not-json").unwrap();

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
        let same_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        for id in ["session-b", "session-a"] {
            store.save(id, "model", &[]);
            std::fs::File::options()
                .write(true)
                .open(dir.path().join(format!("{id}.json")))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(same_time))
                .unwrap();
        }

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

        assert!(store.delete("s1").unwrap());
        assert!(store.load("s1").is_none());
        assert!(!store.delete("s1").unwrap());
        assert!(!store.delete("../../etc/passwd").unwrap());
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
        let store = SessionStore::new(directory.path().to_path_buf());
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
        assert_eq!(first.next_cursor.as_deref(), Some("b.json"));
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
