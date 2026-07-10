//! On-disk persistence for agent conversations, shared by the ACP engine
//! (`src/acp_cmd.rs`, vikunja #961) and the chat REPL (`src/chat_cmd.rs`,
//! vikunja #963). One versioned JSON file per session, keyed by a plain
//! string id, written atomically (temp + rename) so a crash mid-write can't
//! leave a truncated file. Mirrors how Zed's native providers restore full
//! history from a local store.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::providers::{ContentBlock, Message, Role};

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
        let safe =
            !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        safe.then(|| format!("{id}.json"))
    }

    /// Persist a session's history. Best-effort: a write failure is logged to
    /// stderr and never fails the caller's turn.
    pub fn save(&self, id: &str, model: &str, messages: &[Message]) {
        let Some(name) = Self::file_name(id) else { return };
        let record = PersistedSession {
            version: SESSION_PERSIST_VERSION,
            session_id: id.to_string(),
            model: model.to_string(),
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

    /// Summaries of all saved sessions, most-recently-modified first. Files
    /// that don't parse or fail a stat are skipped (best-effort listing).
    pub fn list(&self) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut rows: Vec<(std::time::SystemTime, SessionSummary)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(record) = serde_json::from_slice::<PersistedSession>(&bytes) else { continue };
            if record.version != SESSION_PERSIST_VERSION {
                continue;
            }
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            rows.push((
                mtime,
                SessionSummary {
                    id: record.session_id,
                    model: record.model,
                    message_count: record.messages.len(),
                    first_user_line: first_user_line(&record.messages),
                },
            ));
        }
        rows.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
        rows.into_iter().map(|(_, s)| s).collect()
    }
}

/// First line of the first user text message in `messages`, trimmed — a
/// human-recognizable label for a saved session.
fn first_user_line(messages: &[Message]) -> Option<String> {
    messages.iter().find(|m| m.role == Role::User).and_then(|m| {
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
        vec![Message::user("first question\nsecond line"), Message::assistant("the answer")]
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
        assert!(matches!(&loaded.messages[0].content[0], ContentBlock::Text(t) if t == "first question\nsecond line"));
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
        std::fs::write(dir.path().join("future.json"), serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(store.load("future").is_none(), "an unrecognized version must not be loaded");
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
    fn atomic_write_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.save("s1", "m", &msgs());
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"))
            .collect();
        assert!(leftover.is_empty(), "temp file should have been renamed away");
    }
}
