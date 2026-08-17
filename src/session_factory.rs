//! Production construction of daemon-owned [`SessionCore`] instances.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{AfterHookResult, AgentConfig, AgentSession, TokenLogConfig};
use crate::config::Config;
use crate::providers::{CompleteOpts, LlmProvider, StreamEvent, ThinkingLevel, ToolSchema};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::session_core::{
    ApprovalBroker, CanonicalToolLifecycle, SessionCompaction, SessionCore, SessionEventRouter,
    SessionPersistence,
};
use crate::session_daemon::{SessionFactory, SessionOpenMode};
use crate::session_protocol::{RuntimeChoice, RuntimeOption, RuntimeValue, SessionEvent};
use crate::session_store::SessionStore;

pub type ProviderFactory =
    Arc<dyn Fn() -> Result<Box<dyn LlmProvider>, String> + Send + Sync + 'static>;

pub struct AgentSessionFactory {
    make_provider: ProviderFactory,
    workspace: PathBuf,
    config: Arc<Config>,
    default_model: String,
    models: Vec<String>,
    thinking: ThinkingLevel,
    safety: Arc<SafetyPolicy>,
    token_log: Option<PathBuf>,
    store: SessionStore,
    compaction: SessionCompaction,
    services: Arc<crate::provisioning::ToolServices>,
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
        services: Arc<crate::provisioning::ToolServices>,
    ) -> Self {
        Self {
            make_provider,
            workspace,
            config,
            default_model,
            models,
            thinking,
            safety: Arc::new(safety),
            token_log,
            store,
            compaction,
            services,
        }
    }
}

#[async_trait::async_trait]
impl SessionFactory for AgentSessionFactory {
    async fn open(
        &self,
        session_id: &str,
        mode: SessionOpenMode,
    ) -> Result<Arc<SessionCore>, String> {
        let persisted = match mode {
            SessionOpenMode::Create => None,
            SessionOpenMode::Load => Some(
                self.store
                    .load(session_id)
                    .ok_or_else(|| format!("persisted session '{session_id}' was not found"))?,
            ),
        };
        if persisted
            .as_ref()
            .is_some_and(|record| record.cwd.as_ref() != Some(&self.workspace))
        {
            return Err(format!(
                "persisted session '{session_id}' belongs to a different workspace"
            ));
        }
        let workspace = self.workspace.clone();
        let model = persisted
            .as_ref()
            .map(|record| record.model.clone())
            .unwrap_or_else(|| self.default_model.clone());
        let thinking = persisted
            .as_ref()
            .and_then(|record| record.thinking.as_deref())
            .map(ThinkingLevel::from_input)
            .transpose()?
            .unwrap_or_else(|| self.thinking.clone());
        let provider = (self.make_provider)()?;
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
        let tools = crate::tool_facade::active_schemas(
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
        let persisted_model = model.clone();
        let core = Arc::new(SessionCore::new(
            agent_session,
            model.clone(),
            workspace,
            self.compaction.clone(),
            context_windows,
            approvals,
            Some(SessionPersistence::new(
                session_id.to_string(),
                self.store.clone(),
            )),
            events,
            tool_lifecycle,
        ));
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
            core.persist(&persisted_model, &[], &[]);
        }
        Ok(core)
    }

    fn persisted_session_ids(&self) -> Vec<String> {
        self.store
            .list()
            .into_iter()
            .filter(|summary| summary.cwd.as_ref() == Some(&self.workspace))
            .map(|summary| summary.id)
            .collect()
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
        store.save_acp_with_thinking(
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
            directory.path(),
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
            store,
            SessionCompaction::new(None, false),
            services,
        );
        assert_eq!(
            factory.persisted_session_ids(),
            vec!["session-1".to_string()]
        );

        let core = factory
            .open("session-1", SessionOpenMode::Load)
            .await
            .unwrap();
        let snapshot = core.initial_snapshot("session-1".to_string(), 32).await;

        assert_eq!(snapshot.transcript.len(), 2);
        assert!(snapshot.runtime_options.iter().any(|option| {
            option.id == "thinking" && option.value == RuntimeValue::String("high".to_string())
        }));
        assert_eq!(snapshot.transcript[0].text, "first");
        assert_eq!(snapshot.transcript[1].text, "second");
        assert!(matches!(
            snapshot.transcript[1].outcome,
            Some(crate::session_protocol::AssistantOutcome::Errored {
                ref message,
                ..
            }) if message == "saved failure"
        ));
        assert_eq!(snapshot.tool_calls.len(), 1);
        assert_eq!(
            snapshot.tool_calls[0].status,
            crate::session_protocol::ToolCallStateStatus::Cancelled
        );
        assert_eq!(
            core.current_model
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_str(),
            "saved-model"
        );
    }
}
