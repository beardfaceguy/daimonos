#![allow(dead_code)]

use serde_json::Value;

use crate::protocol::Response;
use crate::providers::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, Message, Role,
    StopReason, ToolSchema, Usage,
};
use crate::session::Session;
use crate::tool_facade;

// --- Hook types ---

pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub enum BeforeHookResult {
    Allow,
    Block(String),
}

pub enum AfterHookResult {
    Continue,
    Terminate,
}

pub type BeforeHook = Box<dyn Fn(&ToolCallInfo) -> BeforeHookResult + Send + Sync>;
pub type AfterHook = Box<dyn Fn(&ToolCallInfo, &str, bool) -> AfterHookResult + Send + Sync>;

// --- Config and Result ---

#[derive(Default)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub opts: CompleteOpts,
    pub before_tool_call: Option<BeforeHook>,
    pub after_tool_call: Option<AfterHook>,
}

pub struct AgentResult {
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}

// --- Pure helpers ---

pub fn accumulate_usage(acc: Usage, turn: Usage) -> Usage {
    Usage {
        input: acc.input + turn.input,
        output: acc.output + turn.output,
        cache_read: acc.cache_read + turn.cache_read,
        cache_write: acc.cache_write + turn.cache_write,
        cost: Cost {
            input_usd: acc.cost.input_usd + turn.cost.input_usd,
            output_usd: acc.cost.output_usd + turn.cost.output_usd,
            cache_read_usd: acc.cost.cache_read_usd + turn.cost.cache_read_usd,
            cache_write_usd: acc.cost.cache_write_usd + turn.cost.cache_write_usd,
            total_usd: acc.cost.total_usd + turn.cost.total_usd,
        },
    }
}

fn response_to_content(resp: Response) -> String {
    if let Some(d) = resp.d {
        serde_json::to_string(&d).unwrap_or_else(|_| "{}".to_string())
    } else if let Some(m) = resp.m {
        m
    } else {
        "{}".to_string()
    }
}

// --- Main loop ---

pub async fn run(
    provider: &dyn LlmProvider,
    session: &mut Session,
    initial_messages: Vec<Message>,
    config: &AgentConfig,
) -> AgentResult {
    let mut messages = initial_messages;
    let mut total_usage = Usage::default();

    loop {
        let ctx = Context {
            messages: messages.clone(),
            system: config.system.clone(),
            tools: config.tools.clone(),
            stable_prefix_len: 0,
        };

        let resp = provider.complete(&ctx, &config.opts).await;
        total_usage = accumulate_usage(total_usage, resp.usage.clone());

        // Assistant turn appended BEFORE tool results (Anthropic API requirement)
        messages.push(Message { role: Role::Assistant, content: resp.content.clone() });

        match resp.stop_reason {
            StopReason::EndTurn
            | StopReason::MaxTokens
            | StopReason::Aborted
            | StopReason::Error => {
                return AgentResult {
                    messages,
                    usage: total_usage,
                    stop_reason: resp.stop_reason,
                    error_message: resp.error_message,
                };
            }
            StopReason::ToolUse => {
                let calls: Vec<_> = resp.content.iter().filter_map(|b| {
                    if let ContentBlock::ToolCall { id, name, input } = b {
                        Some((id.clone(), name.clone(), input.clone()))
                    } else {
                        None
                    }
                }).collect();

                let mut tool_results = Vec::new();
                let mut terminate = false;

                for (id, name, input) in calls {
                    // before_tool_call hook
                    if let Some(hook) = &config.before_tool_call {
                        match hook(&ToolCallInfo { id: id.clone(), name: name.clone(), input: input.clone() }) {
                            BeforeHookResult::Allow => {}
                            BeforeHookResult::Block(reason) => {
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: id,
                                    content: format!("blocked: {reason}"),
                                    is_error: true,
                                });
                                continue;
                            }
                        }
                    }

                    // invoke via facade
                    let (content, is_error) = match tool_facade::invoke(session, &name, &input).await {
                        Some(r) => { let ok = r.ok; (response_to_content(r), !ok) }
                        None => (format!("tool '{name}' not available in agent mode"), true),
                    };

                    // after_tool_call hook
                    if let Some(hook) = &config.after_tool_call {
                        if matches!(
                            hook(&ToolCallInfo { id: id.clone(), name: name.clone(), input: input.clone() }, &content, is_error),
                            AfterHookResult::Terminate
                        ) {
                            terminate = true;
                        }
                    }

                    tool_results.push(ContentBlock::ToolResult { tool_use_id: id, content, is_error });
                }

                if !tool_results.is_empty() {
                    messages.push(Message { role: Role::User, content: tool_results });
                }

                if terminate {
                    return AgentResult {
                        messages,
                        usage: total_usage,
                        stop_reason: StopReason::Aborted,
                        error_message: Some("terminated by after_tool_call hook".to_string()),
                    };
                }
            }
        }
    }
}

// --- Stateful multi-turn session (project #183, task #956) ---

/// One turn's outcome from [`AgentSession::prompt`].
pub struct TurnResult {
    /// Concatenated text of the final assistant message this turn.
    pub text: String,
    /// Token/cost usage for THIS turn.
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}

/// A stateful, re-promptable agent conversation wrapping the one-shot [`run`]
/// loop: holds the provider, the tool `Session`, the loop config (incl. the
/// safety hook), and the running message history + accumulated usage. Shared
/// core for the REPL and ACP frontends (project #183).
pub struct AgentSession {
    provider: Box<dyn LlmProvider>,
    tool_session: Session,
    config: AgentConfig,
    messages: Vec<Message>,
    total_usage: Usage,
}

impl AgentSession {
    pub fn new(provider: Box<dyn LlmProvider>, tool_session: Session, config: AgentConfig) -> Self {
        AgentSession {
            provider,
            tool_session,
            config,
            messages: Vec::new(),
            total_usage: Usage::default(),
        }
    }

    /// Send a user message, run the tool loop to completion, and return this
    /// turn's assistant text + usage. History and accumulated usage persist for
    /// the next prompt.
    ///
    /// Cancel-safe: `self.messages` is only overwritten after `run` completes,
    /// so dropping this future mid-await (e.g. a REPL Ctrl-C abort) leaves the
    /// session's history untouched instead of losing it to a half-finished turn.
    pub async fn prompt(&mut self, user_text: impl Into<String>) -> TurnResult {
        let mut history = self.messages.clone();
        history.push(Message::user(user_text));
        let result = run(self.provider.as_ref(), &mut self.tool_session, history, &self.config).await;
        self.total_usage = accumulate_usage(std::mem::take(&mut self.total_usage), result.usage.clone());
        let text = last_assistant_text(&result.messages);
        self.messages = result.messages;
        TurnResult {
            text,
            usage: result.usage,
            stop_reason: result.stop_reason,
            error_message: result.error_message,
        }
    }

    /// Full conversation history so far.
    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Usage accumulated across every turn this session.
    pub fn total_usage(&self) -> &Usage {
        &self.total_usage
    }

    /// Reset the conversation (e.g. REPL `/clear`); cumulative usage is kept.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Concatenate the `Text` blocks of the last assistant message in `messages`.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::config::Config;
    use crate::providers::LlmResponse;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // --- MockProvider ---

    struct MockProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            MockProvider { responses: Mutex::new(VecDeque::from(responses)) }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            self.responses.lock().unwrap().pop_front()
                .unwrap_or_else(|| LlmResponse::error("MockProvider exhausted"))
        }
    }

    fn mock_usage(input: u64, output: u64) -> Usage {
        Usage { input, output, ..Usage::default() }
    }

    fn end_turn_resp() -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text("done".to_string())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            usage: mock_usage(100, 50),
        }
    }

    fn tool_call_resp(id: &str, name: &str, input: Value) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            error_message: None,
            usage: mock_usage(200, 100),
        }
    }

    fn session_in(dir: &std::path::Path) -> Session {
        Session::new(dir.to_path_buf(), Arc::new(Config::default()))
    }

    // --- AgentSession (multi-turn, project #183) ---

    #[tokio::test]
    async fn session_prompt_returns_assistant_text_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp()]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("hi").await;
        assert_eq!(turn.text, "done");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(sess.history().len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn session_accumulates_history_and_usage_across_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp(), end_turn_resp()]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        sess.prompt("first").await;
        assert_eq!(sess.history().len(), 2);
        sess.prompt("second").await;
        assert_eq!(sess.history().len(), 4, "history must persist across prompts");
        // each end_turn_resp reports mock_usage(100, 50)
        assert_eq!(sess.total_usage().input, 200);
        assert_eq!(sess.total_usage().output, 100);
    }

    #[tokio::test]
    async fn session_tool_call_turn_roundtrips_then_finishes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
        let provider = Box::new(MockProvider::new(vec![
            tool_call_resp("c1", "read_file", json!({"path": "f.txt"})),
            end_turn_resp(),
        ]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("read f.txt").await;
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(turn.text, "done");
        // user, assistant(toolcall), user(toolresult), assistant(end) = 4
        assert_eq!(sess.history().len(), 4);
        assert!(matches!(sess.history()[1].content[0], ContentBlock::ToolCall { .. }));
        assert!(matches!(sess.history()[2].content[0], ContentBlock::ToolResult { .. }));
    }

    #[tokio::test]
    async fn session_clear_resets_history_keeps_usage() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(MockProvider::new(vec![end_turn_resp()]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        sess.prompt("hi").await;
        assert_eq!(sess.history().len(), 2);
        sess.clear();
        assert_eq!(sess.history().len(), 0);
        assert_eq!(sess.total_usage().input, 100, "cumulative usage kept after clear");
    }

    // --- accumulate_usage ---

    #[test]
    fn accumulate_sums_tokens() {
        let total = accumulate_usage(mock_usage(100, 50), mock_usage(200, 75));
        assert_eq!(total.input, 300);
        assert_eq!(total.output, 125);
    }

    #[test]
    fn accumulate_sums_cost() {
        let a = Usage { cost: Cost { input_usd: 1.0, total_usd: 1.5, ..Cost::default() }, ..Usage::default() };
        let b = Usage { cost: Cost { input_usd: 2.0, total_usd: 3.0, ..Cost::default() }, ..Usage::default() };
        let total = accumulate_usage(a, b);
        assert!((total.cost.input_usd - 3.0).abs() < 1e-9);
        assert!((total.cost.total_usd - 4.5).abs() < 1e-9);
    }

    #[test]
    fn accumulate_zero_is_identity() {
        let total = accumulate_usage(mock_usage(500, 250), Usage::default());
        assert_eq!(total.input, 500);
        assert_eq!(total.output, 250);
    }

    // --- response_to_content ---

    #[test]
    fn content_uses_data_field() {
        let resp = Response::ok(json!({"content": "hello"}));
        assert!(response_to_content(resp).contains("hello"));
    }

    #[test]
    fn content_falls_back_to_message() {
        let resp = Response::err(3, "tool failed");
        assert_eq!(response_to_content(resp), "tool failed");
    }

    // --- run loop ---

    #[tokio::test]
    async fn end_turn_stops_loop() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        let result = run(&provider, &mut s, vec![Message::user("hi")], &AgentConfig::default()).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(result.error_message.is_none());
        assert_eq!(result.messages.len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn provider_error_stops_loop_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse::error("API failed")]);
        let result = run(&provider, &mut s, vec![Message::user("hi")], &AgentConfig::default()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("API failed"));
    }

    #[tokio::test]
    async fn max_tokens_stops_loop_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse {
            content: vec![],
            stop_reason: StopReason::MaxTokens,
            error_message: None,
            usage: Usage::default(),
        }]);
        let result = run(&provider, &mut s, vec![Message::user("go")], &AgentConfig::default()).await;
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn usage_accumulates_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            LlmResponse {
                content: vec![ContentBlock::Text("done".into())],
                stop_reason: StopReason::EndTurn,
                error_message: None,
                usage: mock_usage(300, 150),
            },
        ]);
        let result = run(&provider, &mut s, vec![Message::user("go")], &AgentConfig::default()).await;
        assert_eq!(result.usage.input, 500);  // 200 + 300
        assert_eq!(result.usage.output, 250); // 100 + 150
    }

    #[tokio::test]
    async fn assistant_appended_before_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            end_turn_resp(),
        ]);
        let result = run(&provider, &mut s, vec![Message::user("go")], &AgentConfig::default()).await;
        // user, assistant(tool_call), user(tool_result), assistant(end_turn)
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[1].role, Role::Assistant);
        assert!(matches!(&result.messages[1].content[0], ContentBlock::ToolCall { .. }));
        assert_eq!(result.messages[2].role, Role::User);
        assert!(matches!(&result.messages[2].content[0], ContentBlock::ToolResult { .. }));
    }

    #[tokio::test]
    async fn unknown_tool_becomes_is_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "does_not_exist", json!({})),
            end_turn_resp(),
        ]);
        let result = run(&provider, &mut s, vec![Message::user("go")], &AgentConfig::default()).await;
        assert!(matches!(
            &result.messages[2].content[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn real_tool_call_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello agent").unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "read_file", json!({"path": "test.txt"})),
            end_turn_resp(),
        ]);
        let result = run(&provider, &mut s, vec![Message::user("read it")], &AgentConfig::default()).await;
        if let ContentBlock::ToolResult { content, is_error, .. } = &result.messages[2].content[0] {
            assert!(!is_error, "real tool should succeed");
            assert!(content.contains("hello agent"));
        } else {
            panic!("expected ToolResult");
        }
    }

    #[tokio::test]
    async fn before_hook_block_returns_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "exec", json!({"command": "rm -rf /"})),
            end_turn_resp(),
        ]);
        let config = AgentConfig {
            before_tool_call: Some(Box::new(|_| BeforeHookResult::Block("not permitted".into()))),
            ..AgentConfig::default()
        };
        let result = run(&provider, &mut s, vec![Message::user("go")], &config).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(matches!(
            &result.messages[2].content[0],
            ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("blocked")
        ));
    }

    #[tokio::test]
    async fn after_hook_terminate_exits_with_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "nonexistent_tool", json!({})),
            end_turn_resp(), // should not be reached
        ]);
        let config = AgentConfig {
            after_tool_call: Some(Box::new(|_, _, _| AfterHookResult::Terminate)),
            ..AgentConfig::default()
        };
        let result = run(&provider, &mut s, vec![Message::user("go")], &config).await;
        assert_eq!(result.stop_reason, StopReason::Aborted);
        assert!(result.error_message.as_deref().unwrap_or("").contains("terminated"));
    }

    #[tokio::test]
    async fn thinking_blocks_retained_in_assistant_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse {
            content: vec![
                ContentBlock::Thinking("my reasoning".into()),
                ContentBlock::Text("my answer".into()),
            ],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            usage: Usage::default(),
        }]);
        let result = run(&provider, &mut s, vec![Message::user("think")], &AgentConfig::default()).await;
        let assistant = &result.messages[1];
        assert_eq!(assistant.content.len(), 2);
        assert!(matches!(&assistant.content[0], ContentBlock::Thinking(t) if t == "my reasoning"));
    }
}
