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

/// Opt-in one-shot behavior (ADR-036). The default is the behavior of a run
/// without the new options.
#[derive(Default)]
pub struct RunOptions {
    /// Provider name. Used only in the JSON result document.
    pub provider: String,
    pub output: crate::cli::AgentOutputFormat,
}

/// The `--output json` result document of one invocation (ADR-036,
/// Decision 13). Field order is the output order. `schema_version` changes
/// when a field is removed or changes its meaning.
#[derive(Debug, serde::Serialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub session_id: Option<String>,
    pub resumed: bool,
    pub session_saved: Option<bool>,
    pub session_error: Option<String>,
    pub provider: String,
    pub model: String,
    pub stop_reason: String,
    pub error_message: Option<String>,
    pub context_overflow: bool,
    pub final_text: String,
    pub turns: u32,
    pub usage: ReportUsage,
    pub evidence: ReportEvidence,
    pub duration_ms: u64,
}

/// Token usage of this invocation only, with the `agent_runs` column names.
#[derive(Debug, serde::Serialize)]
pub struct ReportUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: Option<u64>,
    pub thinking_bytes: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct ReportEvidence {
    pub status: &'static str,
}

impl RunReport {
    fn new(
        options: &RunOptions,
        model: &str,
        result: &AgentResult,
        initial_len: usize,
        turns: u32,
        elapsed: std::time::Duration,
    ) -> Self {
        // The text blocks of the last assistant message that this invocation
        // added. Earlier messages of a resumed session are not included.
        let final_text = result
            .messages
            .iter()
            .skip(initial_len)
            .rev()
            .find(|message| matches!(message.role, Role::Assistant))
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let status = match result.evidence.status() {
            crate::evidence::EvidenceStatus::Unknown => "unknown",
            crate::evidence::EvidenceStatus::NoMutations => "no_mutations",
            crate::evidence::EvidenceStatus::Verified => "verified",
            crate::evidence::EvidenceStatus::Unverified => "unverified",
        };
        RunReport {
            schema_version: 1,
            session_id: None,
            resumed: false,
            session_saved: None,
            session_error: None,
            provider: options.provider.clone(),
            model: model.to_string(),
            stop_reason: result.stop_reason.as_str().to_string(),
            error_message: result.error_message.clone(),
            context_overflow: result.context_overflow,
            final_text,
            turns,
            usage: ReportUsage {
                input_tokens: result.usage.input,
                output_tokens: result.usage.output,
                reasoning_output_tokens: result.usage.reasoning_output,
                thinking_bytes: result.usage.thinking_bytes,
                cache_read_tokens: result.usage.cache_read,
                cache_write_tokens: result.usage.cache_write,
                cost_usd: result.usage.cost.total_usd,
            },
            evidence: ReportEvidence { status },
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }
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
    run_agent_with(provider, workspace, cfg, args, RunOptions::default(), out).await
}

/// [`run_agent`] with the opt-in one-shot options of ADR-036.
///
/// Context: docs/contexts/session-durability/CONTEXT.md, ADR-036.
pub async fn run_agent_with(
    provider: &dyn LlmProvider,
    workspace: &Path,
    cfg: Arc<Config>,
    args: AgentCmdArgs,
    options: RunOptions,
    out: &mut dyn Write,
) -> Result<AgentResult> {
    let started = std::time::Instant::now();
    let schemas = tool_facade::active_schemas_for_policy(
        workspace,
        &cfg.prompts.resolved_tool_descriptions,
        cfg.filesystem_authority.is_some(),
    );

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
            evidence: Default::default(),
        });
    }

    let mut tools: Vec<ToolSchema> = schemas
        .into_iter()
        .map(|s| ToolSchema {
            name: s.name,
            description: s.description,
            input_schema: s.input_schema,
        })
        .collect();
    // Outbound MCP servers (#1289): one-shot agent mode reads the
    // Claude-style file named by `[agent.mcp]`.
    let native_names: std::collections::HashSet<String> =
        tools.iter().map(|tool| tool.name.clone()).collect();
    let agent_mcp = crate::agent_mcp::connect(&cfg, &native_names, args.analytics.clone()).await;
    if let Some(mcp) = &agent_mcp {
        tools.extend(mcp.tools());
    }

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
        system: Some(crate::prompts::agent_system_for_workspace(&cfg, Some(workspace)).await),
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
        remote_tool_dispatch: agent_mcp.as_ref().map(|mcp| mcp.dispatch_hook()),
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
    let skill_workspace = workspace.to_path_buf();
    let skill_task = args.task.clone();
    let skill_config = cfg.agent.skills.clone();
    let task = tokio::task::spawn_blocking(move || {
        crate::skills::expand_manual_invocation(&skill_workspace, &skill_task, &skill_config, &[])
    })
    .await
    .map_err(|error| anyhow::anyhow!("Agent skill activation failed: {error}"))?
    .map_err(anyhow::Error::msg)?;
    let mut tool_session = Session::new(workspace.to_path_buf(), cfg);
    crate::provisioning::provision_session(&mut tool_session, &services);
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(tool_session));
    let initial = vec![Message::user(task)];
    let initial_len = initial.len();
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
    // Graceful bridge teardown before reporting; bounded by [acp.mcp] budgets.
    if let Some(mcp) = agent_mcp {
        mcp.shutdown().await;
    }
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
    // Messages that this invocation added after its own input. Without a
    // session, the input is one message, so this is the value of before.
    let turns = result.messages.len().saturating_sub(initial_len) as u32;

    if let Some(store) = args.analytics {
        let task_prefix: String = args.task.chars().take(200).collect();
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

    if options.output == crate::cli::AgentOutputFormat::Json {
        // The document is the only stdout write of the run (ADR-036). It is
        // written for each stop reason, before a debug-artifact failure.
        let report = RunReport::new(
            &options,
            &config.opts.model,
            &result,
            initial_len,
            turns,
            started.elapsed(),
        );
        serde_json::to_writer(&mut *out, &report)?;
        writeln!(out)?;
        thought_capture_result?;
        return Ok(result);
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

    // --- ADR-036: --output json ---

    fn json_options() -> RunOptions {
        RunOptions {
            provider: "mock".to_string(),
            output: crate::cli::AgentOutputFormat::Json,
        }
    }

    /// Parse stdout that must be exactly one JSON document and one newline.
    fn single_json_document(out: &[u8]) -> serde_json::Value {
        let text = std::str::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'), "stdout must end with one newline");
        let body = &text[..text.len() - 1];
        assert!(!body.contains('\n'), "stdout must hold one line: {text:?}");
        serde_json::from_str(body).expect("stdout must be one JSON document")
    }

    #[tokio::test]
    async fn output_json_prints_one_document_for_a_successful_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut response = end_turn_with_text("all good");
        response.usage = Usage {
            input: 11,
            output: 7,
            reasoning_output: Some(3),
            // The loop replaces this value with the bytes that the provider
            // streamed as thinking. This mock streams none.
            thinking_bytes: 5,
            cache_read: 13,
            cache_write: 17,
            cost: crate::providers::Cost {
                total_usd: 0.25,
                ..Default::default()
            },
        };
        let provider = MockProvider::new(vec![response]);
        let mut a = args("go");
        a.model = Some("mock-model".to_string());
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            default_cfg(),
            a,
            json_options(),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["session_id"], serde_json::Value::Null);
        assert_eq!(doc["resumed"], false);
        assert_eq!(doc["session_saved"], serde_json::Value::Null);
        assert_eq!(doc["session_error"], serde_json::Value::Null);
        assert_eq!(doc["provider"], "mock");
        assert_eq!(doc["model"], "mock-model");
        assert_eq!(doc["stop_reason"], "end_turn");
        assert_eq!(doc["error_message"], serde_json::Value::Null);
        assert_eq!(doc["context_overflow"], false);
        assert_eq!(doc["final_text"], "all good");
        assert_eq!(doc["turns"], 1);
        assert_eq!(
            doc["usage"],
            serde_json::json!({
                "input_tokens": 11,
                "output_tokens": 7,
                "reasoning_output_tokens": 3,
                "thinking_bytes": 0,
                "cache_read_tokens": 13,
                "cache_write_tokens": 17,
                "cost_usd": 0.25,
            })
        );
        assert!(doc["evidence"]["status"].is_string());
        assert!(doc["duration_ms"].is_u64());
    }

    #[tokio::test]
    async fn output_json_reports_a_provider_error() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::error("api down")]);
        let mut out = Vec::new();
        let result = run_agent_with(
            &provider,
            dir.path(),
            default_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await
        .unwrap();
        // The exit code stays with `check_agent_result`: the result is `Error`.
        assert_eq!(result.stop_reason, crate::providers::StopReason::Error);

        let doc = single_json_document(&out);
        assert_eq!(doc["stop_reason"], "error");
        assert_eq!(doc["error_message"], "api down");
        assert_eq!(doc["context_overflow"], false);
        assert_eq!(doc["final_text"], "");
    }

    #[tokio::test]
    async fn output_json_reports_a_context_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::context_overflow_error(
            "prompt is too long",
        )]);
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            default_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["stop_reason"], "error");
        assert_eq!(doc["context_overflow"], true);
        assert_eq!(doc["error_message"], "prompt is too long");
    }

    #[tokio::test]
    async fn output_json_usage_matches_the_agent_runs_row() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("analytics.db");
        let store = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());
        let mut response = end_turn_with_text("done");
        response.usage = Usage {
            input: 101,
            output: 23,
            reasoning_output: Some(4),
            thinking_bytes: 0,
            cache_read: 3000,
            cache_write: 200,
            cost: crate::providers::Cost {
                total_usd: 0.5,
                ..Default::default()
            },
        };
        let provider = MockProvider::new(vec![response]);
        let mut a = args("go");
        a.analytics = Some(Arc::clone(&store));
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            default_cfg(),
            a,
            json_options(),
            &mut out,
        )
        .await
        .unwrap();
        let doc = single_json_document(&out);

        // Read the row through a second connection, not through the store.
        let db = rusqlite::Connection::open(&db_path).unwrap();
        let row: (i64, i64, Option<i64>, i64, i64, i64, f64, String, i64) = db
            .query_row(
                "SELECT input_tokens, output_tokens, reasoning_output_tokens,
                        thinking_bytes, cache_read_tokens, cache_write_tokens,
                        cost_usd, stop_reason, turns
                 FROM agent_runs",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (101, 23, Some(4), 0, 3000, 200, 0.5, "end_turn".into(), 1)
        );
        assert_eq!(
            doc["usage"],
            serde_json::json!({
                "input_tokens": row.0,
                "output_tokens": row.1,
                "reasoning_output_tokens": row.2,
                "thinking_bytes": row.3,
                "cache_read_tokens": row.4,
                "cache_write_tokens": row.5,
                "cost_usd": row.6,
            })
        );
        assert_eq!(doc["stop_reason"], row.7.as_str());
        assert_eq!(doc["turns"], row.8);
    }

    #[tokio::test]
    async fn output_json_final_text_is_the_last_assistant_message_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "hello\n").unwrap();
        let provider = MockProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![
                    ContentBlock::Text("reading the note".to_string()),
                    ContentBlock::ToolCall {
                        id: "call-1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "note.txt"}),
                    },
                ],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("the note says hello"),
        ]);
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            default_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["final_text"], "the note says hello");
        // assistant (tool call), tool result, assistant (final).
        assert_eq!(doc["turns"], 3);
    }
}
