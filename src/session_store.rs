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

    pub fn save_acp(
        &self,
        id: &str,
        model: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
    ) {
        self.save_record(
            id,
            model,
            None,
            messages,
            Some(cwd.to_path_buf()),
            client_user_message_ids,
            assistant_outcomes,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_acp_with_thinking(
        &self,
        id: &str,
        model: &str,
        thinking: &str,
        messages: &[Message],
        cwd: &Path,
        client_user_message_ids: &[String],
        assistant_outcomes: &[AssistantOutcome],
    ) {
        self.save_record(
            id,
            model,
            Some(thinking.to_string()),
            messages,
            Some(cwd.to_path_buf()),
            client_user_message_ids,
            assistant_outcomes,
        );
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

    fn write_atomic(&self, name: &str, record: &PersistedSession) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        // Write to a temp file then rename, so a crash mid-write can't leave a
        // truncated JSON file that would fail to load.
        let tmp = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, self.dir.join(name))
    }

    /// Load a persisted session, or `None` if absent / unreadable / a version
    /// we don't recognise (all treated as "not resumable", never an error).
    pub fn load(&self, id: &str) -> Option<PersistedSession> {
        let name = Self::file_name(id)?;
        let bytes = std::fs::read(self.dir.join(name)).ok()?;
        let record: PersistedSession = serde_json::from_slice(&bytes).ok()?;
        (record.version == SESSION_PERSIST_VERSION).then_some(record)
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
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut rows: Vec<(std::time::SystemTime, SessionSummary)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<PersistedSession>(&bytes) else {
                continue;
            };
            if record.version != SESSION_PERSIST_VERSION {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            rows.push((
                mtime,
                SessionSummary {
                    id: record.session_id,
                    model: record.model,
                    message_count: record.messages.len(),
                    cwd: record.cwd,
                    updated_at: Some(mtime),
                    first_user_line: first_user_line(&record.messages),
                },
            ));
        }
        rows.sort_by(|(a_time, a_summary), (b_time, b_summary)| {
            b_time
                .cmp(a_time)
                .then_with(|| a_summary.id.cmp(&b_summary.id))
        });
        rows.into_iter().map(|(_, s)| s).collect()
    }
}

/// First line of the first user text message in `messages`, trimmed — a
/// human-recognizable label for a saved session.
fn first_user_line(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text(t) => t.lines().next().map(|l| l.trim().to_string()),
                _ => None,
            })
        })
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
    }

    #[test]
    fn unsafe_id_is_rejected_not_traversed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        // A traversal-ish id must never resolve to a file; save is a no-op and
        // load returns None rather than reading outside the store dir.
        store.save("../../etc/passwd", "m", &msgs());
        assert!(store.load("../../etc/passwd").is_none());
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
}
