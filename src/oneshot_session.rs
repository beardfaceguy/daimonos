//! Named and resumable one-shot `daimonos agent` sessions (ADR-036).
//!
//! Context: docs/contexts/session-durability/CONTEXT.md, ADR-033, ADR-036.
//!
//! A one-shot session is one conversation that a later invocation continues
//! with `--resume`. It has one writer (the ADR-033 owner lock) and its own
//! store directory. A resume must reproduce the request prefix (system prompt,
//! tool list, earlier messages) or refuse: a silently different prefix misses
//! the provider prompt cache that the resume exists for.
//!
//! Owner lock and tool child processes: the store opens the lock file
//! close-on-exec, so a tool child does not keep the lock after its `exec`.
//! Between `fork` and `exec` a child holds a copy of the lock for a moment.
//! A run that ends in that moment can leave the lock held until the child
//! calls `exec` or exits. The next resume is then refused with
//! `session_owned_elsewhere`, and it succeeds on a retry.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::providers::{ContentBlock, Message, Role, ThinkingLevel, ToolSchema};
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
    /// [`endpoint_digest`] of the provider endpoints of the run. A provider
    /// name does not identify an endpoint: two base URLs can have one name.
    /// A digest, not the URL, because a URL can hold credentials.
    #[serde(default)]
    pub endpoint: String,
    /// The system prompt text, sent again as saved. It is never rebuilt,
    /// because the builder reads a workspace that the agent just edited.
    pub system: String,
    /// The tool list in request order.
    pub tools: Vec<ToolDigest>,
}

/// Hex SHA-256 over the base URLs that a run can send the conversation to:
/// the primary base URL, then each secondary provider as (name, base URL), in
/// configuration order. An empty base URL is the default endpoint of its
/// adapter. Surrounding whitespace and trailing slashes do not count.
pub fn endpoint_digest<'a>(
    primary_base_url: &str,
    secondaries: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    fn normal(url: &str) -> &str {
        url.trim().trim_end_matches('/')
    }
    let mut hasher = Sha256::new();
    hasher.update(normal(primary_base_url).as_bytes());
    for (name, base_url) in secondaries {
        hasher.update([0u8]);
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update(normal(base_url).as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The history that a session save stores (ADR-036 Decision 8).
///
/// The run saves for each stop reason, and a resume sends the saved messages
/// again as they are. So the saved history must be one that a provider
/// accepts with a new user message after it:
///
/// 1. Trailing assistant messages without content for the provider are not
///    saved. Such a message has no text other than whitespace, no tool call,
///    and no image: it is empty, or it holds only thinking and provider
///    state. A failed, truncated, or aborted call leaves one. Providers
///    reject it, or an adapter drops it and the request prefix changes.
/// 2. Each tool call has a result. The loop closes open tool calls on its
///    terminal paths. The save does it again, so that the rule does not
///    depend on each return path of the loop.
///
/// Truncated text after `max_tokens` is content and is saved. A history can
/// end with a user message. The next resume then sends two user messages in
/// sequence, which the adapters send in order.
pub fn resumable_history(messages: &[Message]) -> Vec<Message> {
    fn has_provider_content(message: &Message) -> bool {
        message.content.iter().any(|block| match block {
            ContentBlock::Text(text) => !text.trim().is_empty(),
            ContentBlock::ToolCall { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Image { .. } => true,
            ContentBlock::Thinking(_) | ContentBlock::ProviderState { .. } => false,
        })
    }
    let mut kept = messages;
    while let [earlier @ .., last] = kept {
        if last.role == Role::Assistant && !has_provider_content(last) {
            kept = earlier;
        } else {
            break;
        }
    }
    crate::agent::close_orphan_tool_calls(kept.to_vec())
}

/// JSON text with the keys of each object in sorted order.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String(key.clone()),
                        canonical_json(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        serde_json::Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        scalar => scalar.to_string(),
    }
}

/// Digest each tool definition, in request order.
pub fn tool_digests(tools: &[ToolSchema]) -> Vec<ToolDigest> {
    tools
        .iter()
        .map(|tool| {
            let mut hasher = Sha256::new();
            hasher.update(tool.description.as_bytes());
            hasher.update([0u8]);
            // Equal schemas must give equal bytes, also when their keys were
            // inserted in another order and when a build enables the
            // `preserve_order` feature of serde_json. So the keys are sorted
            // here, not left to the map type of `serde_json::Value`.
            hasher.update(canonical_json(&tool.input_schema).as_bytes());
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

    /// The message names no URL: a base URL can hold credentials.
    pub fn endpoint_mismatch(id: &str) -> Self {
        Self::new(
            "session_endpoint_mismatch",
            format!(
                "session {id} was saved with another provider endpoint (base URL) than this \
                 run selects. Start a new session to change the endpoint."
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

    /// Refuse a resume on another model, provider, or endpoint (ADR-036
    /// Decision 5).
    ///
    /// `model` is an explicit `--model` value. A model default of the agent
    /// env file is not a request: the resume uses the saved model. `provider`
    /// is the provider that this run selects (`--provider` or the agent env
    /// file). Its credentials belong to that provider, so a saved session of
    /// another provider is refused and never sent there. `endpoint` is the
    /// [`endpoint_digest`] of this run, for the same reason: one provider
    /// name can have two base URLs.
    pub fn check_overrides(
        &self,
        model: Option<&str>,
        provider: &str,
        endpoint: &str,
    ) -> Result<(), Refusal> {
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
        if endpoint != saved.prefix.endpoint {
            return Err(Refusal::endpoint_mismatch(self.id()));
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
                "tool {} at position {} of the session is not available. If an MCP server \
                 supplies this tool, make sure that the server is connected",
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
            endpoint: "endpoint-a".to_string(),
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
    fn a_resume_on_another_model_provider_or_endpoint_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        saved(dir.path(), "s1");
        let resumed = open_when_free(dir.path(), SessionRequest::Resume("s1".into())).unwrap();

        assert!(resumed.check_overrides(None, "mock", "endpoint-a").is_ok());
        assert!(resumed
            .check_overrides(Some("model-a"), "mock", "endpoint-a")
            .is_ok());
        let model = resumed
            .check_overrides(Some("model-b"), "mock", "endpoint-a")
            .unwrap_err();
        assert_eq!(model.code(), "session_model_mismatch");
        assert!(model.to_string().contains("model-a") && model.to_string().contains("model-b"));
        let provider = resumed
            .check_overrides(None, "another-provider", "endpoint-a")
            .unwrap_err();
        assert_eq!(provider.code(), "session_provider_mismatch");
        let endpoint = resumed
            .check_overrides(None, "mock", "endpoint-b")
            .unwrap_err();
        assert_eq!(endpoint.code(), "session_endpoint_mismatch");
        assert!(!endpoint.to_string().contains("endpoint-b"));
    }

    #[test]
    fn the_endpoint_digest_covers_each_base_url_and_ignores_trailing_slashes() {
        let base = endpoint_digest("https://a.example/v1", [("openai", "")]);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            endpoint_digest(" https://a.example/v1/ ", [("openai", "")])
        );
        for other in [
            endpoint_digest("https://b.example/v1", [("openai", "")]),
            endpoint_digest(
                "https://a.example/v1",
                [("openai", "https://proxy.example")],
            ),
            endpoint_digest("https://a.example/v1", [("anthropic", "")]),
            endpoint_digest("https://a.example/v1", []),
            endpoint_digest("", [("openai", "")]),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn a_record_without_an_endpoint_digest_is_refused_on_resume() {
        // A record of a build before the endpoint digest has no `endpoint`.
        let record: PrefixRecord =
            serde_json::from_str(r#"{"provider":"mock","system":"s","tools":[]}"#).unwrap();
        assert_eq!(record.endpoint, "");
        // The refusal rests on this: a digest is never the empty string, for
        // any input, so an absent digest never equals the digest of a run.
        for digest in [
            endpoint_digest("", []),
            endpoint_digest("  ", [("", "")]),
            endpoint_digest("https://a.example", [("openai", "")]),
        ] {
            assert_eq!(digest.len(), 64);
            assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_ne!(record.endpoint, digest);
        }
    }

    #[test]
    fn a_new_session_accepts_any_model_and_provider() {
        let dir = tempfile::tempdir().unwrap();
        let session = open_when_free(dir.path(), SessionRequest::Start("s1".into())).unwrap();
        assert!(session.check_overrides(Some("any"), "any", "any").is_ok());
    }

    // --- the saved history (ADR-036 Decision 8) ---

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    fn tool_call(id: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "a"}),
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "ok".to_string(),
                is_error: false,
            }],
        }
    }

    fn provider_state() -> ContentBlock {
        ContentBlock::ProviderState {
            provider: "p".to_string(),
            data: serde_json::json!({"signature": "x"}),
        }
    }

    fn shape(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|message| serde_json::to_string(message).unwrap())
            .collect()
    }

    #[test]
    fn a_complete_history_is_saved_unchanged() {
        let history = vec![
            Message::user("task"),
            assistant(vec![
                ContentBlock::Thinking("plan".into()),
                provider_state(),
                tool_call("c1"),
            ]),
            tool_result("c1"),
            Message::assistant("done"),
        ];
        assert_eq!(shape(&resumable_history(&history)), shape(&history));
        // A history that ends with a user message (after a failed call).
        let failed = vec![Message::user("task")];
        assert_eq!(shape(&resumable_history(&failed)), shape(&failed));
        // Truncated text after max_tokens is content for the provider.
        let truncated = vec![Message::user("task"), Message::assistant("partial ans")];
        assert_eq!(shape(&resumable_history(&truncated)), shape(&truncated));
    }

    #[test]
    fn trailing_assistant_messages_without_provider_content_are_not_saved() {
        let kept = vec![Message::user("task"), Message::assistant("answer")];
        for tail in [
            vec![assistant(vec![])],
            vec![assistant(vec![ContentBlock::Thinking(
                "only thoughts".into(),
            )])],
            vec![assistant(vec![provider_state()])],
            vec![assistant(vec![
                ContentBlock::Thinking("t".into()),
                provider_state(),
            ])],
            vec![assistant(vec![ContentBlock::Text("  \n".into())])],
            vec![assistant(vec![]), assistant(vec![provider_state()])],
        ] {
            let mut history = kept.clone();
            history.extend(tail.clone());
            assert_eq!(
                shape(&resumable_history(&history)),
                shape(&kept),
                "tail {tail:?}"
            );
        }
        // Only trailing messages are removed.
        let middle = vec![
            Message::user("task"),
            assistant(vec![]),
            Message::user("again"),
        ];
        assert_eq!(shape(&resumable_history(&middle)), shape(&middle));
    }

    #[test]
    fn each_saved_tool_call_has_a_result() {
        // The loop closes open tool calls before it returns. The save does it
        // again, so the rule does not depend on each return path of the loop.
        let history = vec![
            Message::user("task"),
            assistant(vec![tool_call("c1"), tool_call("c2")]),
            tool_result("c1"),
        ];
        let saved = resumable_history(&history);
        // The existing repair puts the new result message directly after the
        // assistant message, before the result that was already there.
        assert_eq!(shape(&saved[..2]), shape(&history[..2]));
        let mut results: Vec<(String, bool)> = saved[2..]
            .iter()
            .inspect(|message| assert_eq!(message.role, Role::User))
            .flat_map(|message| &message.content)
            .map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => (tool_use_id.clone(), *is_error),
                other => panic!("expected a tool result, got {other:?}"),
            })
            .collect();
        results.sort();
        assert_eq!(
            results,
            vec![("c1".to_string(), false), ("c2".to_string(), true)]
        );

        // A history that ends with an open tool call gets a result message.
        let open_call = vec![Message::user("task"), assistant(vec![tool_call("c9")])];
        let saved = resumable_history(&open_call);
        assert_eq!(saved.len(), 3);
        assert_eq!(saved[2].role, Role::User);
        assert!(matches!(
            &saved[2].content[..],
            [ContentBlock::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "c9"
        ));
    }

    #[test]
    fn a_tool_digest_does_not_depend_on_the_key_order_of_the_schema() {
        let mut forward = serde_json::Map::new();
        forward.insert("type".into(), serde_json::json!("object"));
        forward.insert("properties".into(), serde_json::json!({"b": {}, "a": {}}));
        let mut backward = serde_json::Map::new();
        backward.insert("properties".into(), serde_json::json!({"a": {}, "b": {}}));
        backward.insert("type".into(), serde_json::json!("object"));
        let digest = |schema: serde_json::Map<String, serde_json::Value>| {
            tool_digests(&[tool("t", "d", serde_json::Value::Object(schema))])
        };
        assert_eq!(digest(forward), digest(backward));
        assert_eq!(
            canonical_json(&serde_json::json!({"b": [1, {"z": null, "y": "q\""}], "a": true})),
            r#"{"a":true,"b":[1,{"y":"q\"","z":null}]}"#
        );
    }
}
