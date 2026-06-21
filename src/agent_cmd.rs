#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::agent::{AgentConfig, AgentResult};
use crate::config::Config;
use crate::providers::{CompleteOpts, ContentBlock, LlmProvider, Message, Role, ToolSchema};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::tool_facade;

pub struct AgentCmdArgs {
    pub task: String,
    pub model: Option<String>,
    pub dry_run: bool,
    /// Safety policy (denylist/allowlist/approval mode). `None` = no restrictions.
    pub safety: Option<SafetyPolicy>,
}

/// Run the agent subcommand.
///
/// Takes `args` by value so the optional `SafetyPolicy` can be consumed into
/// the `before_tool_call` hook without cloning.
/// Only assistant text blocks are written to `out`; thinking blocks are suppressed.
pub async fn run_agent(
    provider: &dyn LlmProvider,
    workspace: &Path,
    args: AgentCmdArgs,
    out: &mut dyn Write,
) -> Result<AgentResult> {
    let schemas = tool_facade::active_schemas(workspace);

    if args.dry_run {
        writeln!(out, "[dry-run] task: {}", args.task)?;
        writeln!(out, "[dry-run] {} tool(s) available", schemas.len())?;
        return Ok(AgentResult {
            messages: vec![Message::user(&args.task)],
            usage: Default::default(),
            stop_reason: crate::providers::StopReason::Aborted,
            error_message: Some("dry-run".to_string()),
        });
    }

    let tools: Vec<ToolSchema> = schemas
        .into_iter()
        .map(|s| ToolSchema { name: s.name, description: s.description, input_schema: s.input_schema })
        .collect();

    let model = args.model.unwrap_or_else(|| "claude-opus-4-8".to_string());
    let before_tool_call = args.safety.map(|p| p.into_before_hook());
    let config = AgentConfig {
        system: Some(default_system_prompt()),
        tools,
        opts: CompleteOpts { model, ..CompleteOpts::default() },
        before_tool_call,
        ..AgentConfig::default()
    };

    let cfg = Arc::new(Config::default());
    let mut session = Session::new(workspace.to_path_buf(), cfg);
    let initial = vec![Message::user(&args.task)];
    let result = crate::agent::run(provider, &mut session, initial, &config).await;

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

fn default_system_prompt() -> String {
    "You are Daimonos, an agent-optimized assistant. \
     Use the available tools to complete the task."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
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
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            usage: Usage::default(),
        }
    }

    fn args(task: &str) -> AgentCmdArgs {
        AgentCmdArgs { task: task.to_string(), model: None, dry_run: false, safety: None }
    }

    // --- dry-run ---

    #[tokio::test]
    async fn dry_run_does_not_call_provider() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs { task: "do it".into(), model: None, dry_run: true, safety: None };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), a, &mut out).await.unwrap();
    }

    #[tokio::test]
    async fn dry_run_prints_task_in_output() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs { task: "my task".into(), model: None, dry_run: true, safety: None };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), a, &mut out).await.unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("my task"), "output should mention the task: {s}");
    }

    #[tokio::test]
    async fn dry_run_prints_tool_count() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs { task: "go".into(), model: None, dry_run: true, safety: None };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), a, &mut out).await.unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("tool"), "output should mention tools: {s}");
    }

    // --- run_agent output ---

    #[tokio::test]
    async fn run_prints_assistant_text_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("task complete")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), args("do a thing"), &mut out).await.unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("task complete"), "text block should appear in output: {s}");
    }

    #[tokio::test]
    async fn run_does_not_print_user_message_text() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("assistant reply")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), args("user task prompt"), &mut out).await.unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("user task prompt"), "user message should not appear in output: {s}");
        assert!(s.contains("assistant reply"));
    }

    #[tokio::test]
    async fn run_does_not_print_thinking_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse {
            content: vec![
                ContentBlock::Thinking("internal thoughts".into()),
                ContentBlock::Text("visible answer".into()),
            ],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            usage: Usage::default(),
        }]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), args("think"), &mut out).await.unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("internal thoughts"), "thinking should be hidden: {s}");
        assert!(s.contains("visible answer"));
    }

    // --- model selection ---

    #[tokio::test]
    async fn default_model_is_opus_48() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), args("go"), &mut out).await.unwrap();
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
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), a, &mut out).await.unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].model, "claude-haiku-4-5");
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
        run_agent(&provider, dir.path(), args("check tools"), &mut out).await.unwrap();
        let counts = provider.0.lock().unwrap();
        assert!(*counts.first().unwrap() > 0, "agent loop should include tools in Context");
    }

    // --- error propagation ---

    #[tokio::test]
    async fn provider_error_still_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::error("api down")]);
        let mut out = Vec::new();
        let result = run_agent(&provider, dir.path(), args("go"), &mut out).await.unwrap();
        assert_eq!(result.stop_reason, crate::providers::StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("api down"));
    }

    // --- safety policy wiring ---

    #[tokio::test]
    async fn safety_policy_blocks_denied_tool_before_provider_sees_it() {
        use crate::safety::SafetyPolicy;
        use crate::agent::BeforeHookResult;

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
        assert!(matches!(hook(&info), BeforeHookResult::Block(_)));
    }
}
