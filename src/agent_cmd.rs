#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::Instrument;

use crate::agent::{AgentConfig, AgentResult, TokenLogConfig};
use crate::analytics::{AgentRunRecord, AnalyticsStore};
use crate::config::Config;
use crate::observability::{PromptMetadata, PromptSpan};
use crate::providers::{
    CompleteOpts, ContentBlock, LlmProvider, Message, Role, StreamEvent, ToolSchema,
};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::tool_facade;

pub struct AgentCmdArgs {
    pub task: String,
    pub model: Option<String>,
    pub dry_run: bool,
    /// Safety policy (denylist/allowlist/approval mode). `None` = no restrictions.
    pub safety: Option<SafetyPolicy>,
    /// Analytics store for recording actual token usage. `None` = no recording.
    pub analytics: Option<Arc<AnalyticsStore>>,
    /// `--debug-tokens` log file path. `None` = no per-call token logging.
    pub token_log: Option<std::path::PathBuf>,
    /// Private, truncate-per-run destination for streamed thinking text.
    pub thought_log: Option<std::path::PathBuf>,
    /// Reasoning effort for the model (#1122), from `DAIMONOS_AGENT_THINKING`
    /// (default `medium`). Forwarded into `CompleteOpts.thinking`.
    pub thinking: crate::providers::ThinkingLevel,
}

enum ThoughtCaptureState {
    Active(File),
    Failed(String),
}

struct ThoughtCapture {
    path: std::path::PathBuf,
    state: Mutex<ThoughtCaptureState>,
}

impl ThoughtCapture {
    fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path,
            state: Mutex::new(ThoughtCaptureState::Active(file)),
        })
    }

    fn write_delta(&self, text: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ThoughtCaptureState::Active(file) = &mut *state else {
            return;
        };
        if let Err(error) = file.write_all(text.as_bytes()) {
            *state = ThoughtCaptureState::Failed(error.to_string());
        }
    }

    fn finish(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *state {
            ThoughtCaptureState::Active(file) => file.flush().map_err(|error| {
                anyhow::anyhow!(
                    "failed to flush thought log {}: {error}",
                    self.path.display()
                )
            }),
            ThoughtCaptureState::Failed(error) => Err(anyhow::anyhow!(
                "failed to write thought log {}: {error}",
                self.path.display()
            )),
        }
    }
}

// NOTE: no compaction policy here — ADR-002 compaction operates BETWEEN
// turns of a multi-turn AgentSession, and a one-shot `daimonos agent` run is
// a single turn (there is never an older turn to evict). Intra-run
// compaction for a single long tool loop is explicit ADR-002 future work.

/// Run the agent subcommand.
///
/// Takes `args` by value so the optional `SafetyPolicy` can be consumed into
/// the `before_tool_call` hook without cloning. `cfg` is the already-loaded
/// CLI/project config (vikunja #958) — the tool `Session` is built from it
/// instead of `Config::default()`, so user-configured settings (verbosity,
/// process/extra_path, MCP settings, etc.) apply to agent tool calls too.
/// Only assistant text blocks are written to `out`; thinking blocks are suppressed.
pub async fn run_agent(
    provider: &dyn LlmProvider,
    workspace: &Path,
    cfg: Arc<Config>,
    args: AgentCmdArgs,
    out: &mut dyn Write,
) -> Result<AgentResult> {
    let schemas = tool_facade::active_schemas(workspace, &cfg.prompts.resolved_tool_descriptions);

    if args.dry_run {
        writeln!(out, "[dry-run] task: {}", args.task)?;
        writeln!(out, "[dry-run] {} tool(s) available", schemas.len())?;
        return Ok(AgentResult {
            messages: vec![Message::user(&args.task)],
            usage: Default::default(),
            stop_reason: crate::providers::StopReason::Aborted,
            error_message: Some("dry-run".to_string()),
            last_call_usage: Default::default(),
            context_overflow: false,
        });
    }

    let tools: Vec<ToolSchema> = schemas
        .into_iter()
        .map(|s| ToolSchema {
            name: s.name,
            description: s.description,
            input_schema: s.input_schema,
        })
        .collect();

    let model = args.model.unwrap_or_else(|| "claude-opus-4-8".to_string());
    let before_tool_call = args.safety.map(|p| p.into_before_hook());
    let thought_capture = match args.thought_log {
        Some(path) => Some(Arc::new(ThoughtCapture::open(path.clone()).map_err(
            |error| anyhow::anyhow!("failed to open thought log {}: {error}", path.display()),
        )?)),
        None => None,
    };
    let on_stream_event = thought_capture.as_ref().map(|capture| {
        let capture = Arc::clone(capture);
        Box::new(move |event| {
            if let StreamEvent::ThinkingDelta(text) = event {
                capture.write_delta(&text);
            }
        }) as crate::agent::StreamHook
    });
    let config = AgentConfig {
        system: Some(crate::prompts::agent_system(&cfg).await),
        tools,
        opts: CompleteOpts {
            model,
            thinking: args.thinking,
            ..CompleteOpts::default()
        },
        before_tool_call,
        on_stream_event,
        token_log: args.token_log.map(|path| TokenLogConfig {
            path,
            label: "agent".to_string(),
        }),
        ..AgentConfig::default()
    };

    let services = crate::provisioning::build_tool_services(
        workspace,
        &cfg,
        true,
        false,
        args.analytics.clone(),
    )
    .await;
    let mut tool_session = Session::new(workspace.to_path_buf(), cfg);
    crate::provisioning::provision_session(&mut tool_session, &services);
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(tool_session));
    let initial = vec![Message::user(&args.task)];
    let external_session_id = crate::analytics::read_agent_session_id_env();
    let prompt_span = PromptSpan::new(PromptMetadata {
        mode: "agent",
        session_id: external_session_id.as_deref(),
        model: &config.opts.model,
        workspace,
        turn_index: 0,
        tools_exposed: config.tools.len(),
    });
    let result = crate::agent::run(provider, session, initial, &config)
        .instrument(prompt_span.span().clone())
        .await;
    let thought_capture_result = match thought_capture {
        Some(capture) => capture.finish(),
        None => Ok(()),
    };
    let error_type = match result.stop_reason {
        crate::providers::StopReason::Error => Some("provider_error"),
        crate::providers::StopReason::Refusal => Some("refusal"),
        _ => None,
    };
    prompt_span.finish(result.stop_reason.as_str(), error_type);

    if let Some(store) = args.analytics {
        let task_prefix: String = args.task.chars().take(200).collect();
        let turns = result.messages.len().saturating_sub(1) as u32;
        store.record_agent_run(&AgentRunRecord {
            external_session_id,
            task_prefix,
            input_tokens: result.usage.input,
            output_tokens: result.usage.output,
            reasoning_output_tokens: result.usage.reasoning_output,
            thinking_bytes: result.usage.thinking_bytes,
            cache_read_tokens: result.usage.cache_read,
            cache_write_tokens: result.usage.cache_write,
            cost_usd: result.usage.cost.total_usd,
            stop_reason: result.stop_reason.as_str().to_string(),
            turns,
        });
    }

    // A local debug-artifact failure must be reported, but only after the
    // completed provider run has closed its span and reached analytics.
    thought_capture_result?;

    for msg in &result.messages {
        if matches!(msg.role, Role::Assistant) {
            for block in &msg.content {
                if let ContentBlock::Text(t) = block {
                    writeln!(out, "{t}")?;
                }
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::AnalyticsStore;
    use crate::providers::{CompleteOpts, Context, LlmResponse, Usage};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // --- MockProvider ---

    struct MockProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
        calls: Mutex<Vec<CompleteOpts>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            MockProvider {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_opts(&self) -> Vec<CompleteOpts> {
            self.calls.lock().unwrap().drain(..).collect()
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.calls.lock().unwrap().push(CompleteOpts {
                model: opts.model.clone(),
                max_tokens: opts.max_tokens,
                thinking: opts.thinking.clone(),
                temperature: opts.temperature,
            });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("MockProvider exhausted"))
        }
    }

    struct PanicProvider;

    #[async_trait]
    impl LlmProvider for PanicProvider {
        async fn complete(&self, _: &Context, _: &CompleteOpts) -> LlmResponse {
            panic!("PanicProvider should not be called in dry-run mode")
        }
    }

    fn end_turn_with_text(text: &str) -> LlmResponse {
        LlmResponse {
            retryable: false,
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }
    }

    fn args(task: &str) -> AgentCmdArgs {
        AgentCmdArgs {
            task: task.to_string(),
            model: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        }
    }

    #[tokio::test]
    async fn thought_capture_writes_only_thinking_in_order_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join("thoughts.log");
        let provider = MockProvider::new(vec![LlmResponse {
            retryable: false,
            content: vec![
                ContentBlock::Thinking("first ".to_string()),
                ContentBlock::Text("answer".to_string()),
                ContentBlock::Thinking("second".to_string()),
            ],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let mut capture_args = args("go");
        capture_args.thought_log = Some(path.clone());
        let mut out = Vec::new();

        run_agent(&provider, dir.path(), default_cfg(), capture_args, &mut out)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first second");
        assert_eq!(String::from_utf8(out).unwrap().trim(), "answer");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn thought_capture_surfaces_open_failures_before_provider_call() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().join("not-a-directory");
        std::fs::write(&parent_file, "x").unwrap();
        let path = parent_file.join("thoughts.log");
        let provider = MockProvider::new(vec![end_turn_with_text("unused")]);
        let mut capture_args = args("go");
        capture_args.thought_log = Some(path.clone());
        let mut out = Vec::new();

        let error =
            match run_agent(&provider, dir.path(), default_cfg(), capture_args, &mut out).await {
                Err(error) => error,
                Ok(_) => panic!("invalid thought path must fail"),
            };

        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(provider.call_opts().is_empty());
    }

    #[tokio::test]
    async fn thought_capture_creates_an_empty_file_when_provider_emits_no_thinking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thoughts.log");
        let provider = MockProvider::new(vec![end_turn_with_text("answer")]);
        let mut capture_args = args("go");
        capture_args.thought_log = Some(path.clone());
        let mut out = Vec::new();

        run_agent(&provider, dir.path(), default_cfg(), capture_args, &mut out)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "");
    }

    #[test]
    fn thought_capture_failure_is_checked_after_run_observability() {
        let source = include_str!("agent_cmd.rs");
        let span = source
            .find("prompt_span.finish(result.stop_reason.as_str(), error_type);")
            .unwrap();
        let analytics = source
            .find("store.record_agent_run(&AgentRunRecord")
            .unwrap();
        let capture = source.find("thought_capture_result?;").unwrap();
        assert!(span < capture);
        assert!(analytics < capture);
    }

    fn default_cfg() -> Arc<Config> {
        Arc::new(Config::default())
    }

    // --- dry-run ---

    #[tokio::test]
    async fn dry_run_does_not_call_provider() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs {
            task: "do it".into(),
            model: None,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dry_run_prints_task_in_output() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs {
            task: "my task".into(),
            model: None,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("my task"), "output should mention the task: {s}");
    }

    #[tokio::test]
    async fn dry_run_prints_tool_count() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs {
            task: "go".into(),
            model: None,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("tool"), "output should mention tools: {s}");
    }

    // --- run_agent output ---

    #[tokio::test]
    async fn run_prints_assistant_text_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("task complete")]);
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("do a thing"),
            &mut out,
        )
        .await
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("task complete"),
            "text block should appear in output: {s}"
        );
    }

    #[tokio::test]
    async fn run_does_not_print_user_message_text() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("assistant reply")]);
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("user task prompt"),
            &mut out,
        )
        .await
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("user task prompt"),
            "user message should not appear in output: {s}"
        );
        assert!(s.contains("assistant reply"));
    }

    #[tokio::test]
    async fn run_does_not_print_thinking_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse {
            retryable: false,
            content: vec![
                ContentBlock::Thinking("internal thoughts".into()),
                ContentBlock::Text("visible answer".into()),
            ],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("think"),
            &mut out,
        )
        .await
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("internal thoughts"),
            "thinking should be hidden: {s}"
        );
        assert!(s.contains("visible answer"));
    }

    // --- model selection ---

    #[tokio::test]
    async fn default_model_is_opus_48() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), args("go"), &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].model, "claude-opus-4-8");
    }

    #[tokio::test]
    async fn model_override_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let a = AgentCmdArgs {
            task: "go".into(),
            model: Some("claude-haiku-4-5".into()),
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::Medium,
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].model, "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn thinking_defaults_to_medium_in_complete_opts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), args("go"), &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].thinking, crate::providers::ThinkingLevel::Medium);
    }

    #[tokio::test]
    async fn thinking_level_is_threaded_into_complete_opts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let a = AgentCmdArgs {
            task: "go".into(),
            model: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::High,
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].thinking, crate::providers::ThinkingLevel::High);
    }

    // --- tool schemas ---

    #[tokio::test]
    async fn tools_are_included_in_config() {
        struct SchemaCapture(Mutex<Vec<usize>>);

        #[async_trait]
        impl LlmProvider for SchemaCapture {
            async fn complete(&self, ctx: &Context, _: &CompleteOpts) -> LlmResponse {
                self.0.lock().unwrap().push(ctx.tools.len());
                end_turn_with_text("done")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let provider = SchemaCapture(Mutex::new(Vec::new()));
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("check tools"),
            &mut out,
        )
        .await
        .unwrap();
        let counts = provider.0.lock().unwrap();
        assert!(
            *counts.first().unwrap() > 0,
            "agent loop should include tools in Context"
        );
    }

    // --- token log wiring (--debug-tokens) ---

    #[tokio::test]
    async fn token_log_path_is_wired_and_written() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        let a = AgentCmdArgs {
            task: "go".into(),
            model: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: Some(log_path.clone()),
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"cmd\":\"agent\""));
    }

    #[tokio::test]
    async fn no_token_log_file_written_when_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("go"),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert!(!log_path.exists());
    }

    // --- error propagation ---

    #[tokio::test]
    async fn provider_error_still_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::error("api down")]);
        let mut out = Vec::new();
        let result = run_agent(&provider, dir.path(), default_cfg(), args("go"), &mut out)
            .await
            .unwrap();
        assert_eq!(result.stop_reason, crate::providers::StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("api down"));
    }

    // --- analytics wiring ---

    #[tokio::test]
    async fn analytics_record_agent_run_called_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_analytics.db");
        let store = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());

        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        let a = AgentCmdArgs {
            task: "test analytics wiring".into(),
            model: None,
            dry_run: false,
            safety: None,
            analytics: Some(Arc::clone(&store)),
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();

        let summary = store.agent_runs_summary(1).unwrap();
        assert_eq!(
            summary.total_runs, 1,
            "one agent run must be recorded in analytics"
        );
    }

    #[tokio::test]
    async fn analytics_not_called_on_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_analytics.db");
        let store = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());

        let a = AgentCmdArgs {
            task: "dry run task".into(),
            model: None,
            dry_run: true,
            safety: None,
            analytics: Some(Arc::clone(&store)),
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();

        let summary = store.agent_runs_summary(1).unwrap();
        assert_eq!(
            summary.total_runs, 0,
            "dry-run must not record an agent run"
        );
    }

    // --- safety policy wiring ---

    #[tokio::test]
    async fn safety_policy_blocks_denied_tool_before_provider_sees_it() {
        use crate::agent::BeforeHookResult;
        use crate::safety::SafetyPolicy;

        // Verify that a denied tool gets a Block result from the hook
        let policy = SafetyPolicy {
            denied_commands: vec!["exec".into()],
            ..SafetyPolicy::default()
        };
        let hook = policy.into_before_hook();
        let info = crate::agent::ToolCallInfo {
            id: "t1".into(),
            name: "exec".into(),
            input: serde_json::json!({}),
        };
        assert!(matches!(hook(&info).await, BeforeHookResult::Block(_)));
    }

    #[tokio::test]
    async fn run_agent_execute_script_can_call_builtin_git_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());

        let provider = MockProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "execute_script".into(),
                    input: serde_json::json!({
                        "code": "result = git(command=\"status\")"
                    }),
                }],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("done"),
        ]);
        let mut out = Vec::new();
        let result = run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("check git status"),
            &mut out,
        )
        .await
        .unwrap();

        let tool_result_text = result
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .expect("expected execute_script ToolResult");
        assert!(
            !tool_result_text.contains("plugin not available"),
            "agent execute_script must use the canonical built-in registry: {tool_result_text}"
        );
        let tool_result: serde_json::Value = serde_json::from_str(tool_result_text)
            .expect("execute_script ToolResult should be JSON");
        assert_eq!(
            tool_result["result"]["clean"], true,
            "git status should execute through the plugin: {tool_result_text}"
        );
    }

    // --- cfg wiring (vikunja #958) ---

    // Unix-only: relies on a shebang script + executable mode bit
    // (`PermissionsExt`) to prove PATH resolution through `cfg.process.extra_path`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_agent_uses_provided_cfg_for_tool_session() {
        // The tool `Session` must be built from the `cfg` passed into
        // `run_agent`, not a hardcoded `Config::default()`. Prove it by
        // putting a probe executable only on `cfg.process.extra_path` and
        // having the agent's `exec` tool call resolve it via PATH.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let probe = bin_dir.path().join("cfg_probe_958");
        std::fs::write(&probe, "#!/bin/sh\necho cfg-wired\n").unwrap();
        std::fs::set_permissions(&probe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        let mut cfg = Config::default();
        cfg.process.extra_path = vec![bin_dir.path().to_string_lossy().to_string()];

        let provider = MockProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({"command": "cfg_probe_958"}),
                }],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("done"),
        ]);
        let mut out = Vec::new();
        let result = run_agent(
            &provider,
            dir.path(),
            Arc::new(cfg),
            args("run probe"),
            &mut out,
        )
        .await
        .unwrap();

        let tool_result_text = result
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("expected a ToolResult block");
        assert!(
            tool_result_text.contains("cfg-wired"),
            "exec should resolve cfg_probe_958 via cfg.process.extra_path: {tool_result_text}"
        );
    }
}
