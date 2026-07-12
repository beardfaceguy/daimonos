#![allow(dead_code)]

use std::path::PathBuf;

use serde_json::Value;

use crate::compaction::{self, CompactionEvent, CompactionPolicy, CompactionStrategy};
use crate::protocol::Response;
use crate::providers::{
    CompleteOpts, ContentBlock, Context, Cost, LlmProvider, Message, Role, StopReason, StreamEvent,
    ThinkingLevel, ToolSchema, Usage,
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

/// Async so approval can await a real round-trip (e.g. the ACP engine's
/// `session/request_permission`), not just a local blocking read. The
/// future borrows `info` and is always immediately `.await`ed at the call
/// site, so it doesn't need `'static`.
pub type BeforeHook = Box<
    dyn Fn(
            &ToolCallInfo,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = BeforeHookResult> + Send + '_>>
        + Send
        + Sync,
>;
pub type AfterHook = Box<dyn Fn(&ToolCallInfo, &str, bool) -> AfterHookResult + Send + Sync>;
/// Invoked with each `StreamEvent` as a turn streams in (vikunja #957).
pub type StreamHook = Box<dyn Fn(StreamEvent) + Send + Sync>;
/// Invoked after each compaction (ADR-002) so frontends can surface an
/// informational notice (REPL line, ACP thought chunk).
pub type CompactionHook = Box<dyn Fn(&CompactionEvent) + Send + Sync>;

/// `--debug-tokens` config: where to append one JSON line per LLM API call,
/// and which subcommand (`agent`/`chat`/...) is logging it.
pub struct TokenLogConfig {
    pub path: PathBuf,
    pub label: String,
}

// --- Config and Result ---

#[derive(Default)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub opts: CompleteOpts,
    pub before_tool_call: Option<BeforeHook>,
    pub after_tool_call: Option<AfterHook>,
    pub on_stream_event: Option<StreamHook>,
    pub token_log: Option<TokenLogConfig>,
    /// Context/window compaction (ADR-002). `None` = off.
    pub compaction: Option<CompactionPolicy>,
    pub on_compaction: Option<CompactionHook>,
}

pub struct AgentResult {
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    /// Usage of the loop's FINAL API call — `usage` accumulates every call
    /// in the turn, so only this one's `prompt_tokens()` reflects the
    /// actual window occupancy the compaction trigger needs (ADR-002).
    pub last_call_usage: Usage,
    /// The final call failed as a classified context-window overflow; the
    /// reactive compaction path keys off this to compact and retry once.
    pub context_overflow: bool,
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

/// Render one `--debug-tokens` log line for a single LLM API call.
fn token_log_line(label: &str, model: &str, usage: &Usage) -> String {
    serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "cmd": label,
        "model": model,
        "input": usage.input,
        "output": usage.output,
        "cache_read": usage.cache_read,
        "cache_write": usage.cache_write,
        "cost_usd": usage.cost.total_usd,
    })
    .to_string()
}

/// Best-effort append of one token-usage line. Never panics or propagates
/// I/O errors — a debug log must not be able to break the agent loop.
fn log_token_usage(cfg: &TokenLogConfig, model: &str, usage: &Usage) {
    use std::io::Write;
    let line = token_log_line(&cfg.label, model, usage);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Render one structured compaction event line for the `--debug-tokens`
/// log — the data source for the ADR-002 strategy A/B benchmark.
fn compaction_log_line(label: &str, event: &CompactionEvent) -> String {
    serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "cmd": label,
        "event": "compaction",
        "strategy": event.strategy.as_str(),
        "evicted_turns": event.evicted_turns,
        "evicted_messages": event.evicted_messages,
        "est_tokens_before": event.est_tokens_before,
        "est_tokens_after": event.est_tokens_after,
        "summary_model": event.summary_model,
        "fallback_drop": event.fallback_drop,
    })
    .to_string()
}

/// Best-effort append of one compaction event line (same channel and
/// guarantees as [`log_token_usage`]).
fn log_compaction_event(cfg: &TokenLogConfig, event: &CompactionEvent) {
    use std::io::Write;
    let line = compaction_log_line(&cfg.label, event);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.path)
    {
        let _ = writeln!(f, "{line}");
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

        let resp = match &config.on_stream_event {
            Some(hook) => {
                provider
                    .stream(&ctx, &config.opts, &mut |ev| hook(ev))
                    .await
            }
            None => provider.stream(&ctx, &config.opts, &mut |_| {}).await,
        };
        total_usage = accumulate_usage(total_usage, resp.usage.clone());
        if let Some(log_cfg) = &config.token_log {
            log_token_usage(log_cfg, &config.opts.model, &resp.usage);
        }

        // Assistant turn appended BEFORE tool results (Anthropic API requirement)
        messages.push(Message {
            role: Role::Assistant,
            content: resp.content.clone(),
        });

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
                    last_call_usage: resp.usage,
                    context_overflow: resp.context_overflow,
                };
            }
            StopReason::ToolUse => {
                let calls: Vec<_> = resp
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolCall { id, name, input } = b {
                            Some((id.clone(), name.clone(), input.clone()))
                        } else {
                            None
                        }
                    })
                    .collect();

                let mut tool_results = Vec::new();
                let mut terminate = false;

                for (id, name, input) in calls {
                    // before_tool_call hook
                    if let Some(hook) = &config.before_tool_call {
                        match hook(&ToolCallInfo {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        })
                        .await
                        {
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
                    let (content, is_error) =
                        match tool_facade::invoke(session, &name, &input).await {
                            Some(r) => {
                                let ok = r.ok;
                                (response_to_content(r), !ok)
                            }
                            None => (format!("tool '{name}' not available in agent mode"), true),
                        };

                    // after_tool_call hook
                    if let Some(hook) = &config.after_tool_call {
                        if matches!(
                            hook(
                                &ToolCallInfo {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone()
                                },
                                &content,
                                is_error
                            ),
                            AfterHookResult::Terminate
                        ) {
                            terminate = true;
                        }
                    }

                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error,
                    });
                }

                if !tool_results.is_empty() {
                    messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                }

                if terminate {
                    return AgentResult {
                        messages,
                        usage: total_usage,
                        stop_reason: StopReason::Aborted,
                        error_message: Some("terminated by after_tool_call hook".to_string()),
                        last_call_usage: resp.usage,
                        context_overflow: false,
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
    /// The last API call's measured window occupancy
    /// (`Usage::prompt_tokens()`), read by the proactive compaction trigger
    /// (ADR-002). 0 = not yet measured (fresh/resumed session, or the
    /// provider returned no usage) — the trigger falls back to a chars/4
    /// estimate then.
    last_prompt_tokens: u64,
}

impl AgentSession {
    pub fn new(provider: Box<dyn LlmProvider>, tool_session: Session, config: AgentConfig) -> Self {
        AgentSession {
            provider,
            tool_session,
            config,
            messages: Vec::new(),
            total_usage: Usage::default(),
            last_prompt_tokens: 0,
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
        let user_text: String = user_text.into();

        // Proactive compaction (ADR-002): compact BEFORE the turn when the
        // last measured occupancy (or, if never measured, an estimate)
        // crossed the high-water mark.
        if let Some(policy) = &self.config.compaction {
            let occupancy = if self.last_prompt_tokens > 0 {
                self.last_prompt_tokens
            } else {
                compaction::estimate_prompt_tokens(self.config.system.as_deref(), &self.messages)
            };
            if policy.should_compact(occupancy) {
                self.compact().await;
            }
        }

        let result = self.attempt(&user_text).await;

        // Reactive safety net (ADR-002): a classified context overflow means
        // our between-turns measurement missed — compact and retry ONCE.
        // The failed attempt is not committed, so the retry runs against the
        // freshly compacted history.
        let result = if result.context_overflow
            && self.config.compaction.is_some()
            && self.compact().await.is_some()
        {
            self.attempt(&user_text).await
        } else {
            result
        };

        self.commit(result)
    }

    /// Run one turn attempt against the current history without committing
    /// its messages. Usage and the measured occupancy are recorded either
    /// way (a failed attempt still spent/observed them).
    async fn attempt(&mut self, user_text: &str) -> AgentResult {
        let mut history = self.messages.clone();
        history.push(Message::user(user_text));
        let result = run(
            self.provider.as_ref(),
            &mut self.tool_session,
            history,
            &self.config,
        )
        .await;
        self.total_usage =
            accumulate_usage(std::mem::take(&mut self.total_usage), result.usage.clone());
        self.last_prompt_tokens = result.last_call_usage.prompt_tokens();
        result
    }

    /// Commit an attempt's outcome as this turn's result.
    fn commit(&mut self, result: AgentResult) -> TurnResult {
        let text = last_assistant_text(&result.messages);
        self.messages = result.messages;
        TurnResult {
            text,
            usage: result.usage,
            stop_reason: result.stop_reason,
            error_message: result.error_message,
        }
    }

    /// Compact the history per the configured policy (ADR-002): evict the
    /// oldest turns at a turn boundary, replace them with one summary
    /// message produced by a deterministic one-shot LLM call (retried once;
    /// structural drop with a marker on repeated failure). Returns `None`
    /// when compaction is off or nothing is evictable.
    async fn compact(&mut self) -> Option<CompactionEvent> {
        let policy = self.config.compaction.as_ref()?;
        let cut = compaction::choose_cut(&self.messages, policy.target_tokens())?;
        let summary_model = policy
            .summary_model
            .clone()
            .unwrap_or_else(|| self.config.opts.model.clone());
        let summary_system = policy
            .summary_prompt
            .clone()
            .unwrap_or_else(compaction::default_summary_prompt);

        let est_tokens_before = compaction::estimate_tokens(&self.messages);
        let evicted = &self.messages[..cut];
        let evicted_turns = compaction::turn_starts(evicted).len();
        let transcript = compaction::transcript_for_summary(evicted);

        // One-shot, tool-free, deterministic summarization call. Text-in/
        // text-out so the request itself is never subject to tool-pair
        // validity (see compaction::transcript_for_summary).
        let opts = CompleteOpts {
            model: summary_model.clone(),
            max_tokens: self.config.opts.max_tokens,
            thinking: ThinkingLevel::Off,
            temperature: Some(0.0),
        };
        let ctx = Context {
            messages: vec![Message::user(format!(
                "Summarize this earlier conversation:\n\n{transcript}"
            ))],
            system: Some(summary_system),
            tools: vec![],
            stable_prefix_len: 0,
        };

        // One attempt + one retry (ADR-002 Q4); its tokens count toward the
        // session's cumulative usage like any other call.
        let mut summary_text: Option<String> = None;
        for _ in 0..2 {
            let resp = self.provider.complete(&ctx, &opts).await;
            self.total_usage =
                accumulate_usage(std::mem::take(&mut self.total_usage), resp.usage.clone());
            if let Some(log_cfg) = &self.config.token_log {
                log_token_usage(log_cfg, &summary_model, &resp.usage);
            }
            if resp.error_message.is_none() {
                let text: String = resp
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::Text(t) = b {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    summary_text = Some(text.trim().to_string());
                    break;
                }
            }
        }

        let fallback_drop = summary_text.is_none();
        let replacement = match &summary_text {
            Some(text) => compaction::summary_message(text),
            None => compaction::drop_marker_message(),
        };
        let mut compacted = Vec::with_capacity(1 + self.messages.len() - cut);
        compacted.push(replacement);
        compacted.extend_from_slice(&self.messages[cut..]);
        self.messages = compacted;
        // The measured occupancy no longer describes the compacted history;
        // the next proactive check re-estimates.
        self.last_prompt_tokens = 0;

        let event = CompactionEvent {
            evicted_turns,
            evicted_messages: cut,
            est_tokens_before,
            est_tokens_after: compaction::estimate_tokens(&self.messages),
            summary_model,
            strategy: CompactionStrategy::Summarize,
            fallback_drop,
        };
        if let Some(log_cfg) = &self.config.token_log {
            log_compaction_event(log_cfg, &event);
        }
        if let Some(hook) = &self.config.on_compaction {
            hook(&event);
        }
        Some(event)
    }

    /// Full conversation history so far.
    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Replace the conversation history, e.g. restoring a persisted ACP
    /// session on `session/load` after a process restart. Cumulative usage is
    /// left untouched (it's a fresh process, so there's none to preserve).
    pub fn set_history(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Usage accumulated across every turn this session.
    pub fn total_usage(&self) -> &Usage {
        &self.total_usage
    }

    /// Reset the conversation (e.g. REPL `/clear`); cumulative usage is kept.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Switch the model used for subsequent turns (e.g. the ACP model
    /// picker, vikunja #960). History and cumulative usage are preserved —
    /// only the model sent on the next `prompt` changes.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.opts.model = model.into();
    }

    /// The model currently configured for this session.
    pub fn model(&self) -> &str {
        &self.config.opts.model
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
    use crate::config::Config;
    use crate::providers::LlmResponse;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // --- MockProvider ---

    struct MockProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            MockProvider {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("MockProvider exhausted"))
        }
    }

    fn mock_usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Usage::default()
        }
    }

    fn end_turn_resp() -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text("done".to_string())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
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
            context_overflow: false,
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
        assert_eq!(
            sess.history().len(),
            4,
            "history must persist across prompts"
        );
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
        assert!(matches!(
            sess.history()[1].content[0],
            ContentBlock::ToolCall { .. }
        ));
        assert!(matches!(
            sess.history()[2].content[0],
            ContentBlock::ToolResult { .. }
        ));
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
        assert_eq!(
            sess.total_usage().input,
            100,
            "cumulative usage kept after clear"
        );
    }

    #[tokio::test]
    async fn session_set_model_changes_model_sent_on_next_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            opts: CompleteOpts {
                model: "model-a".to_string(),
                ..CompleteOpts::default()
            },
            ..AgentConfig::default()
        };
        // Capture the model the provider actually sees each turn.
        struct ModelCapture(std::sync::Arc<Mutex<Vec<String>>>);
        #[async_trait]
        impl LlmProvider for ModelCapture {
            async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
                self.0.lock().unwrap().push(opts.model.clone());
                end_turn_resp()
            }
        }
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let provider = Box::new(ModelCapture(std::sync::Arc::clone(&seen)));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), config);
        assert_eq!(sess.model(), "model-a");

        sess.prompt("first").await;
        sess.set_model("model-b");
        assert_eq!(sess.model(), "model-b");
        sess.prompt("second").await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["model-a".to_string(), "model-b".to_string()]
        );
        // History persists across the model switch (user + asst) x2.
        assert_eq!(sess.history().len(), 4);
    }

    // --- token_log (vikunja: --debug-tokens) ---

    #[test]
    fn token_log_line_has_expected_fields() {
        let usage = Usage {
            input: 120,
            output: 45,
            cache_read: 3,
            cache_write: 7,
            cost: Cost {
                total_usd: 0.0012,
                ..Cost::default()
            },
        };
        let line = token_log_line("chat", "claude-haiku-4-5", &usage);
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["cmd"], "chat");
        assert_eq!(parsed["model"], "claude-haiku-4-5");
        assert_eq!(parsed["input"], 120);
        assert_eq!(parsed["output"], 45);
        assert_eq!(parsed["cache_read"], 3);
        assert_eq!(parsed["cache_write"], 7);
        assert_eq!(parsed["cost_usd"], 0.0012);
        assert!(parsed["ts"].is_string(), "must include a timestamp");
    }

    #[test]
    fn log_token_usage_appends_one_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TokenLogConfig {
            path: dir.path().join("tokens.log"),
            label: "agent".to_string(),
        };
        log_token_usage(&cfg, "m1", &mock_usage(10, 5));
        log_token_usage(&cfg, "m1", &mock_usage(20, 8));
        let content = std::fs::read_to_string(&cfg.path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "each call should append exactly one line");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["input"], 10);
        assert_eq!(second["input"], 20);
    }

    #[test]
    fn log_token_usage_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TokenLogConfig {
            path: dir.path().join("nested_does_not_exist_yet.log"),
            label: "agent".to_string(),
        };
        log_token_usage(&cfg, "m1", &mock_usage(1, 1));
        assert!(cfg.path.exists());
    }

    #[tokio::test]
    async fn run_writes_token_log_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        let config = AgentConfig {
            token_log: Some(TokenLogConfig {
                path: log_path.clone(),
                label: "agent".to_string(),
            }),
            ..AgentConfig::default()
        };
        run(&provider, &mut s, vec![Message::user("hi")], &config).await;
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"cmd\":\"agent\""));
    }

    #[tokio::test]
    async fn run_does_not_write_token_log_when_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        run(
            &provider,
            &mut s,
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert!(!log_path.exists());
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
        let a = Usage {
            cost: Cost {
                input_usd: 1.0,
                total_usd: 1.5,
                ..Cost::default()
            },
            ..Usage::default()
        };
        let b = Usage {
            cost: Cost {
                input_usd: 2.0,
                total_usd: 3.0,
                ..Cost::default()
            },
            ..Usage::default()
        };
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

    // --- streaming (vikunja #957) ---

    struct StreamingMockProvider {
        events: Vec<StreamEvent>,
        response: LlmResponse,
    }

    #[async_trait]
    impl LlmProvider for StreamingMockProvider {
        async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            panic!("StreamingMockProvider expects stream(), not complete()");
        }

        async fn stream(
            &self,
            _ctx: &Context,
            _opts: &CompleteOpts,
            on_event: &mut (dyn FnMut(StreamEvent) + Send),
        ) -> LlmResponse {
            for ev in self.events.clone() {
                on_event(ev);
            }
            LlmResponse {
                content: self.response.content.clone(),
                stop_reason: self.response.stop_reason.clone(),
                error_message: self.response.error_message.clone(),
                context_overflow: false,
                usage: self.response.usage.clone(),
            }
        }
    }

    #[tokio::test]
    async fn run_forwards_stream_events_to_hook() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = StreamingMockProvider {
            events: vec![
                StreamEvent::TextDelta("hel".into()),
                StreamEvent::TextDelta("lo".into()),
            ],
            response: end_turn_resp(),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);
        let config = AgentConfig {
            on_stream_event: Some(Box::new(move |ev| seen_clone.lock().unwrap().push(ev))),
            ..AgentConfig::default()
        };
        let result = run(&provider, &mut s, vec![Message::user("hi")], &config).await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            vec![
                StreamEvent::TextDelta("hel".into()),
                StreamEvent::TextDelta("lo".into())
            ]
        );
    }

    #[tokio::test]
    async fn run_without_hook_still_calls_stream_and_ignores_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = StreamingMockProvider {
            events: vec![StreamEvent::TextDelta("x".into())],
            response: end_turn_resp(),
        };
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
    }

    // --- run loop ---

    #[tokio::test]
    async fn end_turn_stops_loop() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![end_turn_resp()]);
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(result.error_message.is_none());
        assert_eq!(result.messages.len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn provider_error_stops_loop_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![LlmResponse::error("API failed")]);
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("hi")],
            &AgentConfig::default(),
        )
        .await;
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
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
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
                context_overflow: false,
                usage: mock_usage(300, 150),
            },
        ]);
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
        assert_eq!(result.usage.input, 500); // 200 + 300
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
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
        // user, assistant(tool_call), user(tool_result), assistant(end_turn)
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[1].role, Role::Assistant);
        assert!(matches!(
            &result.messages[1].content[0],
            ContentBlock::ToolCall { .. }
        ));
        assert_eq!(result.messages[2].role, Role::User);
        assert!(matches!(
            &result.messages[2].content[0],
            ContentBlock::ToolResult { .. }
        ));
    }

    #[tokio::test]
    async fn unknown_tool_becomes_is_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_in(dir.path());
        let provider = MockProvider::new(vec![
            tool_call_resp("t1", "does_not_exist", json!({})),
            end_turn_resp(),
        ]);
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("go")],
            &AgentConfig::default(),
        )
        .await;
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
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("read it")],
            &AgentConfig::default(),
        )
        .await;
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = &result.messages[2].content[0]
        {
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
            before_tool_call: Some(Box::new(|_| {
                Box::pin(std::future::ready(BeforeHookResult::Block(
                    "not permitted".into(),
                )))
            })),
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
        assert!(result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("terminated"));
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
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let result = run(
            &provider,
            &mut s,
            vec![Message::user("think")],
            &AgentConfig::default(),
        )
        .await;
        let assistant = &result.messages[1];
        assert_eq!(assistant.content.len(), 2);
        assert!(matches!(&assistant.content[0], ContentBlock::Thinking(t) if t == "my reasoning"));
    }

    // --- context compaction (ADR-002, vikunja #962) ---

    /// One recorded API call: the model, temperature, and system prompt the
    /// session sent — the compaction tests assert the summarization call's
    /// shape with it.
    type RecordedCall = (String, Option<f64>, Option<String>);

    /// Scripted responses plus per-call recording.
    struct RecordingProvider {
        responses: std::sync::Mutex<VecDeque<LlmResponse>>,
        calls: std::sync::Mutex<Vec<RecordedCall>>,
    }

    impl RecordingProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            RecordingProvider {
                responses: std::sync::Mutex::new(VecDeque::from(responses)),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.calls.lock().unwrap().push((
                opts.model.clone(),
                opts.temperature,
                ctx.system.clone(),
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("RecordingProvider exhausted"))
        }
    }

    /// budget = 800, trigger ≥ 600 measured tokens, evict down to ~400
    /// estimated tokens.
    fn test_policy() -> CompactionPolicy {
        CompactionPolicy {
            high_water: 0.75,
            low_water: 0.5,
            context_window: 1000,
            output_reservation: 200,
            summary_model: None,
            summary_prompt: None,
        }
    }

    fn compaction_config(policy: CompactionPolicy) -> AgentConfig {
        AgentConfig {
            compaction: Some(policy),
            ..AgentConfig::default()
        }
    }

    fn end_turn_with_usage(text: &str, input: u64) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: mock_usage(input, 10),
        }
    }

    /// A user prompt big enough (~1000 estimated tokens) that the chars/4
    /// cut-sizing sees real weight per turn.
    fn big_text(tag: &str) -> String {
        format!("{tag} {}", "x".repeat(4000))
    }

    #[tokio::test]
    async fn proactive_compaction_fires_over_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),         // turn 1: under trigger
            end_turn_with_usage("a2", 700),         // turn 2: measured 700 ≥ 600 → arms trigger
            end_turn_with_usage("the summary", 50), // summarization call
            end_turn_with_usage("a3", 100),         // turn 3 runs on compacted history
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );

        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        let turn = sess.prompt("t3").await;
        assert_eq!(turn.text, "a3");

        // Turn 1 evicted, replaced by the summary; turns 2–3 kept verbatim.
        let history = sess.history();
        assert!(
            matches!(&history[0].content[0], ContentBlock::Text(t) if t.contains("[Summary of earlier conversation: the summary]")),
            "history[0] must be the summary message: {history:?}"
        );
        assert!(
            !history.iter().any(|m| m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("t1 ")))),
            "turn 1 must be evicted"
        );
        assert!(
            history.iter().any(|m| m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("t2 ")))),
            "turn 2 must be kept verbatim"
        );
        assert_eq!(
            history.len(),
            5,
            "summary + turn2 (2 msgs) + turn3 (2 msgs): {history:?}"
        );
    }

    #[tokio::test]
    async fn compaction_disabled_never_compacts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            end_turn_with_usage("a3", 100),
        ]));
        let raw = provider.as_ref() as *const RecordingProvider;
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;
        // SAFETY: provider outlives the session; read-only access for assertion.
        let calls = unsafe { &*raw }.calls();
        assert_eq!(calls.len(), 3, "no summarization call must be made");
        assert_eq!(sess.history().len(), 6, "nothing evicted");
    }

    #[tokio::test]
    async fn reactive_overflow_compacts_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 100), // stays under the proactive trigger
            LlmResponse::context_overflow_error("prompt is too long"),
            end_turn_with_usage("the summary", 50),
            end_turn_with_usage("a3 after retry", 100),
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );

        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        let turn = sess.prompt("t3").await;

        assert_eq!(
            turn.text, "a3 after retry",
            "the retried turn's answer must come back"
        );
        assert!(
            turn.error_message.is_none(),
            "overflow must be recovered, not surfaced"
        );
        let history = sess.history();
        assert!(
            matches!(&history[0].content[0], ContentBlock::Text(t) if t.contains("[Summary of earlier conversation")),
            "reactive path must have compacted: {history:?}"
        );
        // The failed attempt must not have committed its empty assistant msg.
        assert!(
            !history
                .iter()
                .any(|m| m.role == Role::Assistant && m.content.is_empty()),
            "failed overflow attempt must not be committed: {history:?}"
        );
    }

    #[tokio::test]
    async fn overflow_without_policy_surfaces_error_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            LlmResponse::context_overflow_error("prompt is too long"),
        ]));
        let mut sess = AgentSession::new(provider, session_in(dir.path()), AgentConfig::default());
        let turn = sess.prompt("hi").await;
        assert!(
            turn.error_message.is_some(),
            "compaction off → the error surfaces as before"
        );
    }

    #[tokio::test]
    async fn summary_call_uses_summary_model_and_temperature_zero() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            end_turn_with_usage("the summary", 50),
            end_turn_with_usage("a3", 100),
        ]));
        let raw = provider.as_ref() as *const RecordingProvider;
        let policy = CompactionPolicy {
            summary_model: Some("cheap-summarizer".to_string()),
            summary_prompt: Some("Custom summary instructions.".to_string()),
            ..test_policy()
        };
        let mut sess =
            AgentSession::new(provider, session_in(dir.path()), compaction_config(policy));
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;

        let calls = unsafe { &*raw }.calls();
        assert_eq!(calls.len(), 4);
        let (model, temperature, system) = &calls[2]; // the summarization call
        assert_eq!(model, "cheap-summarizer");
        assert_eq!(*temperature, Some(0.0), "summary must be deterministic");
        assert_eq!(system.as_deref(), Some("Custom summary instructions."));
        // Ordinary turn calls keep the session model and no temperature.
        assert_eq!(calls[3].0, CompleteOpts::default().model);
        assert_eq!(calls[3].1, None);
    }

    #[tokio::test]
    async fn summarizer_failure_falls_back_to_drop_marker() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            LlmResponse::error("summarizer down"), // attempt
            LlmResponse::error("still down"),      // retry
            end_turn_with_usage("a3", 100),
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        let turn = sess.prompt("t3").await;

        assert_eq!(
            turn.text, "a3",
            "the session must keep working after the fallback"
        );
        assert!(
            matches!(&sess.history()[0].content[0], ContentBlock::Text(t) if t.contains("summary unavailable")),
            "drop marker must replace the evicted turns: {:?}",
            sess.history()
        );
    }

    #[tokio::test]
    async fn compaction_hook_and_token_log_report_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let events: Arc<std::sync::Mutex<Vec<(usize, bool)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_in_hook = Arc::clone(&events);

        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),
            end_turn_with_usage("a2", 700),
            end_turn_with_usage("the summary", 50),
            end_turn_with_usage("a3", 100),
        ]));
        let config = AgentConfig {
            compaction: Some(test_policy()),
            on_compaction: Some(Box::new(move |event| {
                events_in_hook
                    .lock()
                    .unwrap()
                    .push((event.evicted_turns, event.fallback_drop));
            })),
            token_log: Some(TokenLogConfig {
                path: log_path.clone(),
                label: "test".to_string(),
            }),
            ..AgentConfig::default()
        };
        let mut sess = AgentSession::new(provider, session_in(dir.path()), config);
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;

        let seen = events.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one compaction");
        assert_eq!(seen[0], (1, false), "one turn evicted, no fallback");

        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("\"event\":\"compaction\""),
            "structured event line expected: {log}"
        );
        assert!(log.contains("\"strategy\":\"summarize\""), "{log}");
    }

    #[tokio::test]
    async fn summary_usage_counts_toward_session_total() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Box::new(RecordingProvider::new(vec![
            end_turn_with_usage("a1", 100),         // +100 input
            end_turn_with_usage("a2", 700),         // +700
            end_turn_with_usage("the summary", 40), // +40 (summarization call)
            end_turn_with_usage("a3", 100),         // +100
        ]));
        let mut sess = AgentSession::new(
            provider,
            session_in(dir.path()),
            compaction_config(test_policy()),
        );
        sess.prompt(big_text("t1")).await;
        sess.prompt(big_text("t2")).await;
        sess.prompt("t3").await;
        assert_eq!(
            sess.total_usage().input,
            940,
            "summary call tokens must be counted"
        );
    }
}
