//! Named and resumable one-shot `daimonos agent` sessions (ADR-036).
//!
//! Context: docs/contexts/session-durability/CONTEXT.md, ADR-033, ADR-036.
//!
//! A one-shot session is one conversation that a later invocation continues
//! with `--resume`. It has one writer (the ADR-033 owner lock) and its own
//! store directory. A resume must reproduce the request prefix (system prompt,
//! tool list, earlier messages) or refuse: a silently different prefix misses
//! the provider prompt cache that the resume exists for.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::providers::{Message, ThinkingLevel, ToolSchema};
use crate::session_store::{
    PersistedSession, SessionStore, SessionStoreError, SessionWriteOutcome, SessionWriter,
};
use crate::state_root::RunFailure;

/// Longest accepted session id, in characters (ADR-036 Decision 1).
pub const MAX_SESSION_ID_CHARS: usize = 128;

/// What the caller asked for: `--session-id` or `--resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRequest {
    Start(String),
    Resume(String),
}

impl SessionRequest {
    pub fn id(&self) -> &str {
        match self {
            Self::Start(id) | Self::Resume(id) => id,
        }
    }

    pub fn is_resume(&self) -> bool {
        matches!(self, Self::Resume(_))
    }
}

/// One tool of the request prefix: its name and a digest of its definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDigest {
    pub name: String,
    /// Hex SHA-256 of the description, a zero byte, and the serialized schema.
    pub sha256: String,
}

/// The part of a session record that lets a resume reproduce the request
/// prefix. The model and the thinking level are fields of the record itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixRecord {
    pub provider: String,
    /// The system prompt text, sent again as saved. It is never rebuilt,
    /// because the builder reads a workspace that the agent just edited.
    pub system: String,
    /// The tool list in request order.
    pub tools: Vec<ToolDigest>,
}

/// Digest each tool definition, in request order.
pub fn tool_digests(tools: &[ToolSchema]) -> Vec<ToolDigest> {
    tools
        .iter()
        .map(|tool| {
            let mut hasher = Sha256::new();
            hasher.update(tool.description.as_bytes());
            hasher.update([0u8]);
            // `serde_json::Value` serializes object keys in a stable order,
            // so equal schemas give equal bytes.
            hasher.update(serde_json::to_vec(&tool.input_schema).unwrap_or_default());
            ToolDigest {
                name: tool.name.clone(),
                sha256: hex::encode(hasher.finalize()),
            }
        })
        .collect()
}

/// A session request that the run refuses before it calls the model. The
/// process exits with code 2 and the `run_configuration_invalid` JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub RunFailure);

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for Refusal {}

impl From<RunFailure> for Refusal {
    fn from(failure: RunFailure) -> Self {
        Self(failure)
    }
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self(RunFailure::configuration_invalid(code, message))
    }

    #[cfg(test)]
    pub fn code(&self) -> &'static str {
        self.0.code
    }

    pub fn state_unusable(message: impl Into<String>) -> Self {
        Self(RunFailure::state_unusable(message))
    }

    pub fn model_mismatch(id: &str, saved: &str, requested: &str) -> Self {
        Self::new(
            "session_model_mismatch",
            format!(
                "session {id} was saved with model {saved}; --model {requested} differs. \
                 Start a new session to change the model."
            ),
        )
    }

    pub fn provider_mismatch(id: &str, saved: &str, requested: &str) -> Self {
        Self::new(
            "session_provider_mismatch",
            format!(
                "session {id} was saved with provider {saved}; this run selects provider \
                 {requested}. Start a new session to change the provider."
            ),
        )
    }
}

/// Check the id before any store access (ADR-036 Decision 1).
pub fn validate_id(id: &str) -> Result<(), Refusal> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if id.is_empty() || id.chars().count() > MAX_SESSION_ID_CHARS || !id.chars().all(allowed) {
        return Err(Refusal::new(
            "session_id_invalid",
            format!(
                "a session id has 1 to {MAX_SESSION_ID_CHARS} characters from A-Z, a-z, 0-9, \
                 '-' and '_'"
            ),
        ));
    }
    Ok(())
}

/// The saved state that a resume continues.
#[derive(Debug, Clone)]
pub struct SavedSession {
    pub model: String,
    pub thinking: ThinkingLevel,
    pub messages: Vec<Message>,
    pub prefix: PrefixRecord,
}

/// A claimed session: this process is its one writer until the value drops.
pub struct OpenSession {
    request: SessionRequest,
    store: SessionStore,
    writer: SessionWriter,
    next_generation: u64,
    saved: Option<SavedSession>,
}

impl OpenSession {
    pub fn id(&self) -> &str {
        self.request.id()
    }

    pub fn is_resume(&self) -> bool {
        self.request.is_resume()
    }

    /// The saved state of a resumed session. `None` for a new session.
    pub fn saved(&self) -> Option<&SavedSession> {
        self.saved.as_ref()
    }

    /// Refuse a resume on another model or provider (ADR-036 Decision 5).
    ///
    /// `model` is an explicit `--model` value. A model default of the agent
    /// env file is not a request: the resume uses the saved model. `provider`
    /// is the provider that this run selects (`--provider` or the agent env
    /// file). Its credentials belong to that provider, so a saved session of
    /// another provider is refused and never sent there.
    pub fn check_overrides(&self, model: Option<&str>, provider: &str) -> Result<(), Refusal> {
        let Some(saved) = &self.saved else {
            return Ok(());
        };
        if provider != saved.prefix.provider {
            return Err(Refusal::provider_mismatch(
                self.id(),
                &saved.prefix.provider,
                provider,
            ));
        }
        match model {
            Some(model) if model != saved.model => {
                Err(Refusal::model_mismatch(self.id(), &saved.model, model))
            }
            _ => Ok(()),
        }
    }

    /// Refuse a resume whose tool list differs from the saved prefix
    /// (ADR-036 Decision 5). A new session always passes.
    pub fn check_tools(&self, tools: &[ToolSchema]) -> Result<(), Refusal> {
        let Some(saved) = &self.saved else {
            return Ok(());
        };
        let current = tool_digests(tools);
        let saved_tools = &saved.prefix.tools;
        let first_difference = saved_tools
            .iter()
            .zip(&current)
            .position(|(saved, current)| saved != current);
        let detail = match first_difference {
            Some(index) if saved_tools[index].name == current[index].name => format!(
                "the definition of tool {} at position {index} changed",
                current[index].name
            ),
            Some(index) => format!(
                "position {index} has tool {}, the session has {}",
                current[index].name, saved_tools[index].name
            ),
            None if current.len() > saved_tools.len() => format!(
                "tool {} at position {} is not in the session",
                current[saved_tools.len()].name,
                saved_tools.len()
            ),
            None if current.len() < saved_tools.len() => format!(
                "tool {} at position {} of the session is not available",
                saved_tools[current.len()].name,
                current.len()
            ),
            None => return Ok(()),
        };
        Err(Refusal::new(
            "session_tools_changed",
            format!(
                "session {} cannot continue with a different tool list: {detail}",
                self.id()
            ),
        ))
    }

    /// Save the conversation through the claimed writer at the next
    /// generation. Any result other than a completed save is an error
    /// (ADR-036 Decision 8).
    pub fn save(
        &self,
        model: &str,
        thinking: &str,
        messages: &[Message],
        cwd: &Path,
        prefix: &PrefixRecord,
    ) -> Result<(), String> {
        match self.store.save_oneshot_generation_result(
            &self.writer,
            self.next_generation,
            model,
            thinking,
            messages,
            cwd,
            prefix,
        ) {
            Ok(SessionWriteOutcome::Saved(_)) => Ok(()),
            Ok(SessionWriteOutcome::Stale { stored_generation }) => Err(format!(
                "the session changed during the run (stored generation {stored_generation})"
            )),
            Ok(SessionWriteOutcome::Superseded) => {
                Err("another writer took the session during the run".to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Claim a session for this process (ADR-036 Decision 4).
///
/// A new session claims the writer with no expected generation, so an existing
/// record refuses it. A resume loads the record and claims the writer at the
/// loaded generation. The claim takes the ADR-033 owner lock.
pub fn open(store: SessionStore, request: SessionRequest) -> Result<OpenSession, Refusal> {
    let id = request.id().to_string();
    validate_id(&id)?;
    let (expected_generation, saved) = match &request {
        SessionRequest::Start(_) => (None, None),
        SessionRequest::Resume(_) => {
            let record = store
                .load_result(&id)
                .map_err(|error| store_refusal(&id, error))?;
            let generation = record.generation;
            (Some(generation), Some(saved_session(&id, record)?))
        }
    };
    let writer = store
        .claim_writer(&id, expected_generation)
        .map_err(|error| store_refusal(&id, error))?;
    Ok(OpenSession {
        request,
        store,
        writer,
        next_generation: expected_generation.unwrap_or(0).saturating_add(1),
        saved,
    })
}

fn saved_session(id: &str, record: PersistedSession) -> Result<SavedSession, Refusal> {
    let Some(prefix) = record.prefix else {
        return Err(Refusal::new(
            "session_not_resumable",
            format!("session {id} has no prefix record; it was not saved by a one-shot run"),
        ));
    };
    let thinking = record
        .thinking
        .as_deref()
        .and_then(|level| ThinkingLevel::from_input(level).ok());
    let Some(thinking) = thinking else {
        return Err(Refusal::new(
            "session_not_resumable",
            format!("session {id} has no usable thinking level"),
        ));
    };
    Ok(SavedSession {
        model: record.model,
        thinking,
        messages: record.messages,
        prefix,
    })
}

fn store_refusal(id: &str, error: SessionStoreError) -> Refusal {
    match error {
        SessionStoreError::UnsafeId => Refusal::new(
            "session_id_invalid",
            format!("session id {id} is not valid"),
        ),
        SessionStoreError::AlreadyExists => Refusal::new(
            "session_exists",
            format!("session {id} exists; use --resume {id} to continue it"),
        ),
        SessionStoreError::NotFound => Refusal::new(
            "session_not_found",
            format!("session {id} does not exist; use --session-id {id} to start it"),
        ),
        SessionStoreError::OwnedElsewhere { pid } => Refusal::new(
            "session_owned_elsewhere",
            match pid {
                Some(pid) => format!("session {id} is in use by process {pid}"),
                None => format!("session {id} is in use by another process"),
            },
        ),
        SessionStoreError::WriterChanged { .. } => Refusal::new(
            "session_changed",
            format!("session {id} changed while this run started; try again"),
        ),
        error @ (SessionStoreError::FutureVersion { .. }
        | SessionStoreError::UnsupportedVersion { .. }
        | SessionStoreError::Corrupt(_)) => Refusal::new(
            "session_not_resumable",
            format!("session {id} cannot be read by this build: {error}"),
        ),
        error @ (SessionStoreError::Database(_) | SessionStoreError::Io(_)) => {
            Refusal::state_unusable(format!("one-shot session store: {error}"))
        }
    }
}

/// Test helper: [`open`], with a bounded wait while the owner lock is held.
///
/// The owner lock is a `flock`. A child that another test thread forked keeps
/// the lock's open file description until it calls `exec`, so in the
/// multi-threaded test binary a lock can outlive a dropped session for a
/// moment. Tests that are not about lock ownership wait for that moment.
#[cfg(test)]
pub(crate) fn open_when_free(dir: &Path, request: SessionRequest) -> Result<OpenSession, Refusal> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match open(SessionStore::new(dir.to_path_buf()), request.clone()) {
            Err(refusal)
                if refusal.code() == "session_owned_elsewhere"
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, schema: serde_json::Value) -> ToolSchema {
        ToolSchema {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
        }
    }

    fn tools() -> Vec<ToolSchema> {
        vec![
            tool("alpha", "first", serde_json::json!({"type": "object"})),
            tool("beta", "second", serde_json::json!({"type": "object"})),
        ]
    }

    fn prefix(tools: &[ToolSchema]) -> PrefixRecord {
        PrefixRecord {
            provider: "mock".to_string(),
            system: "system text".to_string(),
            tools: tool_digests(tools),
        }
    }

    fn store(dir: &Path) -> SessionStore {
        SessionStore::new(dir.to_path_buf())
    }

    /// Start a session and save one exchange, then release the writer.
    fn saved(dir: &Path, id: &str) {
        let session = open(store(dir), SessionRequest::Start(id.to_string())).unwrap();
        session
            .save(
                "model-a",
                "medium",
                &[Message::user("task"), Message::assistant("done")],
                dir,
                &prefix(&tools()),
            )
            .unwrap();
    }

    #[test]
    fn id_rules_are_checked_before_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        for id in ["", "has space", "dot.dot", "a/b", &"x".repeat(129)] {
            let error = open(store(&missing), SessionRequest::Start(id.to_string()))
                .err()
                .unwrap();
            assert_eq!(error.code(), "session_id_invalid", "id {id:?}");
            assert_eq!(error.0.exit_code, 2);
        }
        assert!(!missing.exists(), "an invalid id must not touch the store");
        assert!(validate_id(&"x".repeat(128)).is_ok());
        assert!(validate_id("run-42_code").is_ok());
    }

    #[test]
    fn a_new_session_saves_and_a_resume_loads_it() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");

        let resumed = open_when_free(dir.path(), SessionRequest::Resume("s1".into())).unwrap();
        let state = resumed.saved().unwrap();
        assert_eq!(state.model, "model-a");
        assert_eq!(state.thinking.as_str(), "medium");
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.prefix, prefix(&tools()));
        assert!(resumed.is_resume());
    }

    #[test]
    fn starting_an_existing_id_is_refused_and_keeps_the_record() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");
        let before = store(dir.path()).load_result("s1").unwrap();

        let error = open_when_free(dir.path(), SessionRequest::Start("s1".into()))
            .err()
            .unwrap();
        assert_eq!(error.code(), "session_exists");

        let after = store(dir.path()).load_result("s1").unwrap();
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.messages.len(), before.messages.len());
    }

    #[test]
    fn resuming_an_unknown_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = open_when_free(dir.path(), SessionRequest::Resume("nope".into()))
            .err()
            .unwrap();
        assert_eq!(error.code(), "session_not_found");
    }

    #[test]
    fn a_record_without_a_prefix_is_not_resumable() {
        let dir = tempfile::tempdir().unwrap();
        // A chat-style record: same store type, no prefix record.
        store(dir.path()).save("chat-1", "model-a", &[Message::user("hi")]);
        let error = open_when_free(dir.path(), SessionRequest::Resume("chat-1".into()))
            .err()
            .unwrap();
        assert_eq!(error.code(), "session_not_resumable");
    }

    #[test]
    fn a_second_live_owner_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");
        let _first = open(store(dir.path()), SessionRequest::Resume("s1".into())).unwrap();
        let error = open(store(dir.path()), SessionRequest::Resume("s1".into()))
            .err()
            .unwrap();
        assert_eq!(error.code(), "session_owned_elsewhere");
    }

    #[test]
    fn each_save_advances_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");
        let first = store(dir.path()).load_result("s1").unwrap().generation;
        {
            let resumed = open_when_free(dir.path(), SessionRequest::Resume("s1".into())).unwrap();
            resumed
                .save(
                    "model-a",
                    "medium",
                    &[Message::user("task"), Message::assistant("done")],
                    dir.path(),
                    &prefix(&tools()),
                )
                .unwrap();
        }
        let second = store(dir.path()).load_result("s1").unwrap().generation;
        assert_eq!(second, first + 1);
    }

    #[test]
    fn tool_list_differences_are_refused_and_name_the_first_tool() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");
        let resumed = open_when_free(dir.path(), SessionRequest::Resume("s1".into())).unwrap();
        assert!(resumed.check_tools(&tools()).is_ok());

        let mut reordered = tools();
        reordered.reverse();
        let mut changed = tools();
        changed[1].description = "second, edited".to_string();
        let mut schema_changed = tools();
        schema_changed[0].input_schema = serde_json::json!({"type": "object", "x": 1});
        let mut extra = tools();
        extra.push(tool("gamma", "third", serde_json::json!({})));
        let fewer = vec![tools().remove(0)];
        for (case, list, names) in [
            ("order", reordered, "beta"),
            ("description", changed, "beta"),
            ("schema", schema_changed, "alpha"),
            ("extra", extra, "gamma"),
            ("fewer", fewer, "beta"),
        ] {
            let error = resumed.check_tools(&list).err().unwrap();
            assert_eq!(error.code(), "session_tools_changed", "{case}");
            assert!(
                error.0.message.contains(names),
                "{case}: {}",
                error.0.message
            );
        }
    }

    #[test]
    fn a_new_session_accepts_any_tool_list() {
        let dir = tempfile::tempdir().unwrap();
        let session = open_when_free(dir.path(), SessionRequest::Start("s1".into())).unwrap();
        assert!(session.check_tools(&tools()).is_ok());
        assert!(session.saved().is_none());
    }

    #[test]
    fn a_resume_on_another_model_or_provider_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");
        let resumed = open_when_free(dir.path(), SessionRequest::Resume("s1".into())).unwrap();
        let saved_provider = resumed.saved().unwrap().prefix.provider.clone();

        assert!(resumed.check_overrides(None, &saved_provider).is_ok());
        assert!(resumed
            .check_overrides(Some("model-a"), &saved_provider)
            .is_ok());
        let model = resumed
            .check_overrides(Some("model-b"), &saved_provider)
            .unwrap_err();
        assert_eq!(model.code(), "session_model_mismatch");
        assert!(model.to_string().contains("model-a") && model.to_string().contains("model-b"));
        let provider = resumed
            .check_overrides(None, "another-provider")
            .unwrap_err();
        assert_eq!(provider.code(), "session_provider_mismatch");
    }

    #[test]
    fn a_new_session_accepts_any_model_and_provider() {
        let dir = tempfile::tempdir().unwrap();
        let session = open_when_free(dir.path(), SessionRequest::Start("s1".into())).unwrap();
        assert!(session.check_overrides(Some("any"), "any").is_ok());
    }
}
