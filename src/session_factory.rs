//! Production construction of daemon-owned [`SessionCore`] instances.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{AfterHookResult, AgentConfig, AgentSession, TokenLogConfig};
use crate::analytics::AnalyticsStore;
use crate::config::Config;
use crate::providers::{CompleteOpts, LlmProvider, StreamEvent, ThinkingLevel, ToolSchema};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::session_core::{
    ApprovalBroker, CanonicalToolLifecycle, PersistenceRetryPolicy, SessionCompaction, SessionCore,
    SessionEventRouter, SessionPersistence,
};
use crate::session_daemon::{SessionFactory, SessionOpenError, SessionOpenMode};
use crate::session_protocol::{
    RuntimeChoice, RuntimeOption, RuntimeValue, SessionEvent, SessionWorkspace,
};
use crate::session_store::{
    SessionStore, SessionStoreError, SessionStoreErrorKind, SessionSummaryScan,
};
use sha2::{Digest, Sha256};

pub type ProviderFactory =
    Arc<dyn Fn() -> Result<Box<dyn LlmProvider>, String> + Send + Sync + 'static>;

pub struct AgentSessionFactory {
    make_provider: ProviderFactory,
    workspace: PathBuf,
    workspace_identity: SessionWorkspace,
    config: Arc<Config>,
    default_model: String,
    models: Vec<String>,
    thinking: ThinkingLevel,
    safety: Arc<SafetyPolicy>,
    token_log: Option<PathBuf>,
    store: SessionStore,
    compaction: SessionCompaction,
    analytics: Option<Arc<AnalyticsStore>>,
    services: Arc<crate::provisioning::ToolServices>,
    catalog_writer: Option<Arc<crate::session_catalog::SessionCatalogWriter>>,
}

impl AgentSessionFactory {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        make_provider: ProviderFactory,
        workspace: PathBuf,
        config: Arc<Config>,
        default_model: String,
        models: Vec<String>,
        thinking: ThinkingLevel,
        safety: SafetyPolicy,
        token_log: Option<PathBuf>,
        store: SessionStore,
        compaction: SessionCompaction,
        analytics: Option<Arc<AnalyticsStore>>,
        services: Arc<crate::provisioning::ToolServices>,
    ) -> Self {
        let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        let workspace_identity =
            canonical_session_workspace(&workspace, config.session.max_label_bytes);
        Self {
            make_provider,
            workspace,
            workspace_identity,
            config,
            default_model,
            models,
            thinking,
            safety: Arc::new(safety),
            token_log,
            store,
            compaction,
            analytics,
            services,
            catalog_writer: None,
        }
    }

    pub fn with_catalog_writer(
        mut self,
        catalog_writer: Arc<crate::session_catalog::SessionCatalogWriter>,
    ) -> Self {
        self.catalog_writer = Some(catalog_writer);
        self
    }
}

#[async_trait::async_trait]
impl SessionFactory for AgentSessionFactory {
    async fn open(
        &self,
        session_id: &str,
        mode: SessionOpenMode,
    ) -> Result<Arc<SessionCore>, SessionOpenError> {
        let persisted = match mode {
            SessionOpenMode::Create => None,
            SessionOpenMode::Load => Some(self.store.load_result(session_id).map_err(|error| {
                log_load_error(session_id, &error);
                session_open_error(error.kind())
            })?),
        };
        if let Some(record) = persisted.as_ref() {
            let cwd = record
                .cwd
                .as_deref()
                .ok_or(SessionOpenError::WorkspaceUnknown)?;
            let canonical_cwd = std::fs::canonicalize(cwd).map_err(|error| {
                let store_error = SessionStoreError::Io(error);
                log_load_error(session_id, &store_error);
                session_open_error(store_error.kind())
            })?;
            if canonical_cwd != self.workspace {
                return Err(SessionOpenError::WorkspaceMismatch);
            }
        }
        let persistence = SessionPersistence::claim(
            session_id.to_string(),
            self.store.clone(),
            persisted.as_ref().map(|record| record.generation),
            PersistenceRetryPolicy::new(
                self.config.session.persistence_retry_attempts,
                std::time::Duration::from_millis(
                    self.config.session.persistence_retry_initial_backoff_ms,
                ),
                std::time::Duration::from_millis(
                    self.config.session.persistence_retry_max_backoff_ms,
                ),
            ),
        )
        .map_err(|error| {
            tracing::warn!(
                target: "daimonos::session_factory",
                event = "session_writer_claim_failed",
                session_id,
                error = %error,
            );
            SessionOpenError::Io
        })?;
        let workspace = self.workspace.clone();
        let model = persisted
            .as_ref()
            .map(|record| record.model.clone())
            .unwrap_or_else(|| self.default_model.clone());
        let thinking = persisted
            .as_ref()
            .and_then(|record| record.thinking.as_deref())
            .and_then(|raw| match ThinkingLevel::from_input(raw) {
                Ok(thinking) => Some(thinking),
                Err(_) => {
                    tracing::warn!(
                        target: "daimonos::session_factory",
                        event = "persisted_thinking_invalid",
                        session_id,
                        "falling back to configured thinking level",
                    );
                    None
                }
            })
            .unwrap_or_else(|| self.thinking.clone());
        let provider = (self.make_provider)().map_err(|_error| {
            tracing::error!(
                target: "daimonos::session_factory",
                event = "session_provider_create_failed",
                session_id,
                "provider creation failed while opening session"
            );
            SessionOpenError::Internal
        })?;
        let events = Arc::new(SessionEventRouter::new_with_replay(
            None,
            self.config.session.replay_events,
        ));
        let approvals = Arc::new(ApprovalBroker::new_with_timeout(
            true,
            std::time::Duration::from_secs(self.config.session.approval_timeout_secs),
        ));
        let tool_lifecycle = Arc::new(CanonicalToolLifecycle::new_with_output_limit(
            Arc::clone(&events),
            Arc::clone(&approvals),
            Arc::clone(&self.safety),
            self.config.session.max_active_tool_calls,
            self.config.session.max_tool_event_output_bytes,
        ));
        let before_lifecycle = Arc::clone(&tool_lifecycle);
        let after_lifecycle = Arc::clone(&tool_lifecycle);
        let stream_events = Arc::clone(&events);
        let mut tools: Vec<ToolSchema> = crate::tool_facade::active_schemas(
            &workspace,
            &self.config.prompts.resolved_tool_descriptions,
        )
        .into_iter()
        .map(|schema| ToolSchema {
            name: schema.name,
            description: schema.description,
            input_schema: schema.input_schema,
        })
        .collect();
        // Outbound MCP servers (#1289). The bridge is kept alive by the
        // dispatch hook's Arc; on session drop the client pool reaps stdio
        // children (bounded teardown, #1293) — no explicit shutdown hook
        // exists on this path yet.
        let native_names: std::collections::HashSet<String> =
            tools.iter().map(|tool| tool.name.clone()).collect();
        let agent_mcp =
            crate::agent_mcp::connect(&self.config, &native_names, self.analytics.clone()).await;
        if let Some(mcp) = &agent_mcp {
            tools.extend(mcp.tools());
        }
        let agent_config = AgentConfig {
            system: Some(crate::prompts::agent_system(&self.config).await),
            tools,
            opts: CompleteOpts {
                model: model.clone(),
                thinking: thinking.clone(),
                ..CompleteOpts::default()
            },
            before_tool_call: Some(Box::new(move |info| {
                let lifecycle = Arc::clone(&before_lifecycle);
                Box::pin(async move { lifecycle.before(info).await })
            })),
            after_tool_call: Some(Box::new(move |info, output, is_error| {
                after_lifecycle.finish(info, output, is_error);
                AfterHookResult::Continue
            })),
            on_stream_event: Some(Box::new(move |event| {
                let event = match event {
                    StreamEvent::TextDelta(text) => SessionEvent::AssistantDelta { text },
                    StreamEvent::ThinkingDelta(text) => SessionEvent::ThoughtDelta { text },
                };
                let _ = stream_events.emit(event);
            })),
            token_log: self.token_log.clone().map(|path| TokenLogConfig {
                path,
                label: "session-daemon".to_string(),
            }),
            compaction: self.compaction.policy.clone(),
            remote_tool_dispatch: agent_mcp.as_ref().map(|mcp| mcp.dispatch_hook()),
            ..AgentConfig::default()
        };
        let mut tool_session = Session::new(workspace.clone(), Arc::clone(&self.config));
        crate::provisioning::provision_session(&mut tool_session, &self.services);
        tool_session.external_session_id = Some(session_id.to_string());
        let mut agent_session = AgentSession::new(provider, tool_session, agent_config);
        if let Some(record) = persisted.as_ref() {
            agent_session.set_history(record.messages.clone());
        }
        // Dynamic model windows must be resolved for the selected/restored
        // model on first use; the startup policy may belong to a different
        // default model.
        let context_windows = HashMap::new();
        let core = Arc::new(SessionCore::new(
            agent_session,
            model.clone(),
            workspace,
            self.compaction.clone(),
            context_windows,
            approvals,
            Some(match &self.catalog_writer {
                Some(writer) => persistence.with_catalog_writer(Arc::clone(writer)),
                None => persistence,
            }),
            events,
            tool_lifecycle,
        ));
        if let Some(record) = persisted.as_ref() {
            core.initialize_persistence_generation(record.generation);
        }
        core.set_runtime_options(runtime_options(
            &self.models,
            &core.current_model(),
            &thinking,
        ));
        if let Some(record) = persisted {
            *core.client_user_message_ids.lock().await = record.client_user_message_ids;
            *core
                .assistant_outcomes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = record.assistant_outcomes;
        } else {
            core.persist_current().await;
        }
        Ok(core)
    }

    async fn scan_persisted_summaries(
        &self,
        max_preview_bytes: usize,
        after_name: Option<String>,
        max_entries: usize,
        max_duration: std::time::Duration,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> SessionSummaryScan {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let fallback_cursor = after_name.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut scan = store.scan_summaries(
                max_preview_bytes,
                after_name.as_deref(),
                max_entries,
                std::time::Instant::now() + max_duration,
            );
            let mut workspace_complete = true;
            scan.summaries.retain(|summary| {
                let Some(cwd) = summary.cwd.as_deref() else {
                    return false;
                };
                match std::fs::canonicalize(cwd) {
                    Ok(cwd) => cwd == workspace,
                    Err(error) => {
                        workspace_complete = false;
                        tracing::warn!(
                            target: "daimonos::session_factory",
                            event = "session_summary_workspace_failed",
                            session_id = %summary.id,
                            error = %error,
                            "persisted session workspace could not be resolved"
                        );
                        false
                    }
                }
            });
            scan.complete &= workspace_complete;
            scan
        })
        .await
        .unwrap_or(SessionSummaryScan {
            summaries: Vec::new(),
            next_cursor: fallback_cursor,
            complete: false,
        })
    }

    async fn missing_persisted_ids(&self, session_ids: Vec<String>) -> Vec<String> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            session_ids
                .into_iter()
                .filter(|session_id| persisted_id_is_missing(&store, session_id))
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    fn workspace_identity(&self) -> Option<SessionWorkspace> {
        Some(self.workspace_identity.clone())
    }
}

impl From<SessionStoreErrorKind> for SessionOpenError {
    fn from(kind: SessionStoreErrorKind) -> Self {
        match kind {
            SessionStoreErrorKind::UnsafeId => Self::UnsafeId,
            SessionStoreErrorKind::AlreadyExists => {
                debug_assert!(false, "session open cannot produce duplicate-import errors");
                Self::Internal
            }
            SessionStoreErrorKind::WriterChanged => Self::Io,
            SessionStoreErrorKind::NotFound => Self::NotFound,
            SessionStoreErrorKind::FutureVersion => Self::FutureVersion,
            SessionStoreErrorKind::UnsupportedVersion => Self::UnsupportedVersion,
            SessionStoreErrorKind::Corrupt => Self::Corrupt,
            SessionStoreErrorKind::Permission => Self::Permission,
            SessionStoreErrorKind::Database | SessionStoreErrorKind::Io => Self::Io,
        }
    }
}

fn session_open_error(kind: SessionStoreErrorKind) -> SessionOpenError {
    kind.into()
}

fn persisted_id_is_missing(store: &SessionStore, session_id: &str) -> bool {
    matches!(
        store.load_result(session_id),
        Err(SessionStoreError::NotFound | SessionStoreError::UnsafeId)
    )
}

fn log_load_error(session_id: &str, error: &SessionStoreError) {
    if error.kind() == SessionStoreErrorKind::NotFound {
        return;
    }
    tracing::warn!(
        target: "daimonos::session_factory",
        event = "persisted_session_open_failed",
        session_id,
        error_kind = ?error.kind(),
        error = %error,
        "persisted session could not be opened"
    );
}

pub(crate) fn canonical_session_workspace(
    workspace: &std::path::Path,
    max_label_bytes: usize,
) -> SessionWorkspace {
    let mut digest = Sha256::new();
    digest.update(workspace.as_os_str().as_bytes());
    let digest = format!("{:x}", digest.finalize());
    let label = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let label_end = crate::plugins::floor_char_boundary(label, max_label_bytes);
    SessionWorkspace {
        id: format!("ws_{}", &digest[..32]),
        label: label[..label_end].to_string(),
    }
}

fn runtime_options(
    models: &[String],
    current_model: &str,
    thinking: &ThinkingLevel,
) -> Vec<RuntimeOption> {
    let mut candidates = vec![current_model.to_string()];
    for model in models {
        if !candidates.contains(model) {
            candidates.push(model.clone());
        }
    }
    vec![
        RuntimeOption::select(
            "model",
            "Model",
            RuntimeValue::String(current_model.to_string()),
            candidates
                .into_iter()
                .map(|model| RuntimeChoice::new(model.clone(), model))
                .collect(),
        ),
        RuntimeOption::select(
            "thinking",
            "Thinking",
            RuntimeValue::String(thinking.as_str().to_string()),
            ThinkingLevel::ALL
                .iter()
                .map(|level| RuntimeChoice::new(level.as_str(), level.as_str()))
                .collect(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ContentBlock, Context, LlmResponse, StopReason, Usage};

    struct StaticProvider;

    #[async_trait::async_trait]
    impl LlmProvider for StaticProvider {
        async fn complete(&self, _context: &Context, _options: &CompleteOpts) -> LlmResponse {
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::Text("pong".to_string())],
                stop_reason: StopReason::EndTurn,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            }
        }
    }

    #[tokio::test]
    async fn created_core_executes_and_persists_completed_turn() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let config = Arc::new(Config::default());
        let services = Arc::new(
            crate::provisioning::build_tool_services(directory.path(), &config, true, true, None)
                .await,
        );
        let factory = AgentSessionFactory::new(
            Arc::new(|| Ok(Box::new(StaticProvider))),
            directory.path().to_path_buf(),
            config,
            "test-model".to_string(),
            vec!["test-model".to_string()],
            ThinkingLevel::default(),
            SafetyPolicy::default(),
            None,
            store.clone(),
            SessionCompaction::new(None, false),
            None,
            services,
        );
        let core = factory
            .open("session-1", SessionOpenMode::Create)
            .await
            .unwrap();

        core.prompt(
            crate::providers::Message::user("ping"),
            "ping".to_string(),
            Some("prompt-1".to_string()),
            None,
            || {},
            |_| crate::session_protocol::AssistantOutcome::Completed,
        )
        .await
        .unwrap();

        let persisted = store.load("session-1").expect("completed turn persisted");
        assert_eq!(persisted.model, "test-model");
        assert_eq!(persisted.thinking.as_deref(), Some("medium"));
        assert_eq!(persisted.messages.len(), 2);
    }

    #[tokio::test]
    async fn loaded_core_seeds_reconnect_snapshot_from_persisted_history() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        store.save_acp(
            "session-1",
            "saved-model",
            "high",
            &[
                crate::providers::Message::user("first"),
                crate::providers::Message::assistant("second"),
                crate::providers::Message {
                    role: crate::providers::Role::Assistant,
                    content: vec![crate::providers::ContentBlock::ToolCall {
                        id: "dangling".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "README.md"}),
                    }],
                },
            ],
            &directory.path().join("."),
            &["prompt-1".to_string()],
            &[crate::session_protocol::AssistantOutcome::Errored {
                context_overflow: false,
                message: "saved failure".to_string(),
            }],
        );
        let foreign = directory.path().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        store.save_acp(
            "foreign-session",
            "saved-model",
            "medium",
            &[crate::providers::Message::user("foreign")],
            &foreign,
            &[],
            &[],
        );
        let config = Arc::new(Config::default());
        let services = Arc::new(
            crate::provisioning::build_tool_services(directory.path(), &config, true, true, None)
                .await,
        );
        let factory = AgentSessionFactory::new(
            Arc::new(|| Ok(Box::new(StaticProvider))),
            directory.path().to_path_buf(),
            config,
            "default-model".to_string(),
            vec!["default-model".to_string(), "saved-model".to_string()],
            ThinkingLevel::default(),
            SafetyPolicy::default(),
            None,
            store.clone(),
            SessionCompaction::new(None, false),
            None,
            services,
        );
        let scan = factory
            .scan_persisted_summaries(
                256,
                None,
                100,
                std::time::Duration::from_secs(1),
                Arc::new(tokio::sync::Semaphore::new(1))
                    .acquire_owned()
                    .await
                    .unwrap(),
            )
            .await;
        assert!(scan.complete);
        let summaries = scan.summaries;
        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.id.as_str())
                .collect::<Vec<_>>(),
            vec!["session-1"]
        );
        assert_eq!(summaries[0].model, "saved-model");
        assert_eq!(summaries[0].message_count, 3);
        assert_eq!(summaries[0].first_user_line.as_deref(), Some("first"));
        assert!(factory.workspace_identity().is_some());

        let core = factory
            .open("session-1", SessionOpenMode::Load)
            .await
            .unwrap();
        let snapshot = core.initial_snapshot("session-1".to_string(), 32).await;

        assert_eq!(snapshot.timeline.len(), 4);
        assert!(snapshot.runtime_options.iter().any(|option| {
            option.id == "thinking" && option.value == RuntimeValue::String("high".to_string())
        }));
        assert!(matches!(
            &snapshot.timeline[0].entry,
            crate::session_protocol::TimelineEntryKind::User { text, .. } if text == "first"
        ));
        assert!(matches!(
            &snapshot.timeline[1].entry,
            crate::session_protocol::TimelineEntryKind::Assistant { text, .. } if text == "second"
        ));
        assert!(matches!(
            snapshot.timeline[2].entry,
            crate::session_protocol::TimelineEntryKind::Tool {
                status: crate::session_protocol::ToolCallStateStatus::Cancelled,
                ..
            }
        ));
        assert!(matches!(
            &snapshot.timeline[3].entry,
            crate::session_protocol::TimelineEntryKind::Outcome {
                outcome: crate::session_protocol::AssistantOutcome::Errored { message, .. }
            } if message == "saved failure"
        ));
        assert!(snapshot.active_tools.is_empty());
        assert_eq!(
            core.current_model
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_str(),
            "saved-model"
        );

        store.save_acp(
            "invalid-thinking",
            "saved-model",
            "removed-level",
            &[crate::providers::Message::user("still loadable")],
            directory.path(),
            &[],
            &[],
        );
        let invalid = factory
            .open("invalid-thinking", SessionOpenMode::Load)
            .await
            .expect("invalid persisted thinking falls back");
        let snapshot = invalid
            .initial_snapshot("invalid-thinking".to_string(), 32)
            .await;
        assert!(snapshot.runtime_options.iter().any(|option| {
            option.id == "thinking" && option.value == RuntimeValue::String("medium".to_string())
        }));

        assert!(matches!(
            factory.open("foreign-session", SessionOpenMode::Load).await,
            Err(SessionOpenError::WorkspaceMismatch)
        ));
        store.save("corrupt", "saved-model", &[]);
        store
            .replace_payload_for_test("corrupt", b"{not-json")
            .unwrap();
        assert!(matches!(
            factory.open("corrupt", SessionOpenMode::Load).await,
            Err(SessionOpenError::Corrupt)
        ));
        assert!(matches!(
            factory.open("missing", SessionOpenMode::Load).await,
            Err(SessionOpenError::NotFound)
        ));

        store.save("workspace-unknown", "saved-model", &[]);
        assert!(matches!(
            factory
                .open("workspace-unknown", SessionOpenMode::Load)
                .await,
            Err(SessionOpenError::WorkspaceUnknown)
        ));
        store.save_acp(
            "workspace-unavailable",
            "saved-model",
            "medium",
            &[],
            &directory.path().join("removed-workspace"),
            &[],
            &[],
        );
        assert!(matches!(
            factory
                .open("workspace-unavailable", SessionOpenMode::Load)
                .await,
            Err(SessionOpenError::Io)
        ));
        assert!(persisted_id_is_missing(&store, "../../unsafe"));
        assert!(!persisted_id_is_missing(&store, "corrupt"));
    }
}
