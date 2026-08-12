//! Context/window compaction (ADR-002, vikunja #962).
//!
//! Long conversations grow `AgentSession`'s history unbounded; once
//! `system + history + new message` exceeds the model's context window the
//! API rejects the request. Compaction replaces the *older* turns with one
//! small summary message so the prompt fits again, transparently to all
//! frontends. This module holds the pure machinery — policy math, token
//! estimation, turn segmentation, cut selection, transcript rendering,
//! replacement-message construction; the LLM summarization call itself
//! lives in `AgentSession::compact` (src/agent.rs), which owns the provider.
//!
//! Everything numeric comes from required agent-env config (no defaults in
//! code — see ADR-002 "Configuration"); this module only computes with the
//! values it's given.

use crate::providers::{ContentBlock, Message, Role};

/// Compaction thresholds and summarizer settings, parsed from the agent env
/// file (`agent_env.rs`). Present = compaction on; `AgentConfig.compaction`
/// is `None` when off.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPolicy {
    /// Trigger fraction of the budget (e.g. 0.75): compact before the next
    /// turn when the last turn's measured prompt occupancy reaches it.
    pub high_water: f64,
    /// Target fraction of the budget (e.g. 0.50): evict oldest turns until
    /// the estimated kept tail is near it.
    pub low_water: f64,
    /// The model's context window, in tokens.
    pub context_window: u64,
    /// Tokens reserved for the reply (the window is shared by input+output).
    pub output_reservation: u64,
    /// Summarizer model; `None` → the session's main model.
    pub summary_model: Option<String>,
    /// Summarization system prompt; `None` → [`default_summary_prompt`].
    pub summary_prompt: Option<String>,
}

impl CompactionPolicy {
    /// Tokens available for the prompt: window minus the reply reservation.
    pub fn budget(&self) -> u64 {
        self.context_window.saturating_sub(self.output_reservation)
    }

    /// Should we compact before the next turn, given the last measured (or
    /// estimated) prompt occupancy in tokens?
    pub fn should_compact(&self, prompt_tokens: u64) -> bool {
        prompt_tokens as f64 >= self.high_water * self.budget() as f64
    }

    /// The size the kept tail should be compacted down to.
    pub fn target_tokens(&self) -> u64 {
        (self.low_water * self.budget() as f64) as u64
    }
}

/// The strategy seam (ADR-002): how evicted turns become their replacement.
/// MVP ships `Summarize` only; the future `Spill` strategy (external chunk
/// store + in-context pointers) adds a variant here plus the env selector
/// key — deliberately not exposed as config until a second variant exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    Summarize,
}

impl CompactionStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactionStrategy::Summarize => "summarize",
        }
    }
}

/// One compaction occurrence, for frontend notices (`on_compaction` hook)
/// and the `--debug-tokens` structured event line.
#[derive(Debug, Clone)]
pub struct CompactionEvent {
    pub evicted_turns: usize,
    pub evicted_messages: usize,
    pub est_tokens_before: u64,
    pub est_tokens_after: u64,
    pub summary_model: String,
    pub strategy: CompactionStrategy,
    /// True when the summarizer failed (after one retry) and the evicted
    /// turns were structurally dropped with a marker instead.
    pub fallback_drop: bool,
}

/// Default summarization system prompt. Externalized to `prompts/summary.md`
/// and embedded at compile time (vikunja #974); overridable at runtime via the
/// agent-env `DAIMONOS_AGENT_SUMMARY_PROMPT` or the `[prompts].summary` config
/// key (see `prompts::apply_summary_override`).
pub fn default_summary_prompt() -> String {
    crate::prompts::SUMMARY_DEFAULT.to_string()
}

// --- Token estimation (chars/4) ---

/// Rough tokens-per-character divisor for English/code. Used only to *size
/// the cut* (per-message counts don't exist on the wire — real usage is
/// whole-conversation) and as the trigger fallback when no measured usage
/// exists yet (freshly-resumed session, usage-less self-hosted shims). The
/// trigger itself uses the exact measured `Usage::prompt_tokens()`.
const EST_CHARS_PER_TOKEN: u64 = 4;

/// Estimate the token footprint of one message (all block payloads).
fn estimate_message_tokens(message: &Message) -> u64 {
    let chars: usize = message
        .content
        .iter()
        .map(|b| match b {
            ContentBlock::Text(t) | ContentBlock::Thinking(t) => t.len(),
            ContentBlock::ProviderState { provider, data } => {
                provider.len() + data.to_string().len()
            }
            ContentBlock::Image {
                data,
                media_type,
                uri,
            } => data.len() + media_type.len() + uri.as_ref().map_or(0, String::len),
            ContentBlock::ToolCall { id, name, input } => {
                id.len() + name.len() + input.to_string().len()
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => tool_use_id.len() + content.len(),
        })
        .sum();
    chars as u64 / EST_CHARS_PER_TOKEN
}

/// Estimate the token footprint of a message slice.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Estimate for the whole prompt: system + history. Used as the trigger
/// fallback when no measured usage exists yet.
pub fn estimate_prompt_tokens(system: Option<&str>, messages: &[Message]) -> u64 {
    system
        .map(|s| s.len() as u64 / EST_CHARS_PER_TOKEN)
        .unwrap_or(0)
        + estimate_tokens(messages)
}

// --- Turn segmentation & cut selection ---

/// A *genuine* user-text message starts a turn. Tool results also arrive as
/// User-role messages but start with a `ToolResult` block, so checking the
/// first block distinguishes them.
fn is_turn_start(message: &Message) -> bool {
    message.role == Role::User
        && matches!(
            message.content.first(),
            Some(ContentBlock::Text(_) | ContentBlock::Image { .. })
        )
}

/// Indices of every turn start in `messages`.
pub fn turn_starts(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_turn_start(m))
        .map(|(i, _)| i)
        .collect()
}

/// Choose the eviction cut: evict `messages[..cut]`, keep `messages[cut..]`.
///
/// The cut always lands on a turn start, which guarantees the hard API
/// constraint (ADR-002 Q3): no kept `ToolResult` references an evicted
/// `ToolCall` and vice versa, and the kept tail begins with a real user
/// message. Walking newest→oldest, the kept tail grows until it *reaches*
/// the target — the straddling turn is kept (round outward), never split.
/// The most recent turn is always kept regardless of size.
///
/// Returns `None` when there is nothing to evict (fewer than two turns, or
/// the whole history already fits the target).
pub fn choose_cut(messages: &[Message], target_tokens: u64) -> Option<usize> {
    let starts = turn_starts(messages);
    if starts.len() < 2 {
        return None;
    }
    // Start from the newest turn (always kept), extend the kept tail older
    // while it hasn't yet reached the target. Checking BEFORE extending
    // keeps the turn the target lands in (round outward) and stops at the
    // newest turn when that alone already exceeds the target.
    let mut cut = *starts.last().expect("len checked above");
    for &start in starts.iter().rev().skip(1) {
        if estimate_tokens(&messages[cut..]) >= target_tokens {
            break;
        }
        cut = start;
    }
    (cut > 0).then_some(cut)
}

// --- Transcript & replacement messages ---

/// Per-tool-result cap in the summarizer's transcript. Tool results carry
/// facts the summary must preserve, but full outputs would blow up the
/// summarization call itself; the head is where errors/answers usually are.
const SUMMARY_TOOL_RESULT_CAP: usize = 500;

/// Render evicted turns as a plain-text transcript for the summarization
/// call. Text-in/text-out deliberately: passing the real `Message` structs
/// would make the summarization request itself subject to tool-pair/schema
/// validity (fragile, provider-specific). Unlike the chat REPL's display
/// transcript, this one *includes* (capped) tool results — the summary
/// prompt asks for facts learned from them.
pub fn transcript_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::Text(text) => {
                    let label = match message.role {
                        Role::User => "User",
                        Role::Assistant => "Assistant",
                    };
                    out.push_str(&format!("{label}: {text}\n"));
                }
                ContentBlock::Image {
                    media_type, uri, ..
                } => {
                    let location = uri
                        .as_deref()
                        .map(|uri| format!(" {uri}"))
                        .unwrap_or_default();
                    out.push_str(&format!("[user image: {media_type}{location}]\n"));
                }
                ContentBlock::ToolCall { name, input, .. } => {
                    out.push_str(&format!("[tool call: {name} {input}]\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let capped: String = content.chars().take(SUMMARY_TOOL_RESULT_CAP).collect();
                    let ellipsis = if content.chars().count() > SUMMARY_TOOL_RESULT_CAP {
                        "…"
                    } else {
                        ""
                    };
                    let tag = if *is_error {
                        "tool error"
                    } else {
                        "tool result"
                    };
                    out.push_str(&format!("[{tag}: {capped}{ellipsis}]\n"));
                }
                // Thinking/provider continuation state is internal; visible
                // assistant text restates anything that mattered.
                ContentBlock::Thinking(_) | ContentBlock::ProviderState { .. } => {}
            }
        }
    }
    out
}

/// The synthetic message that replaces the evicted turns. `User` role
/// because the first post-system message must be `user` (Anthropic), and
/// consecutive user messages are accepted by both dialects.
pub fn summary_message(summary_text: &str) -> Message {
    Message::user(format!("[Summary of earlier conversation: {summary_text}]"))
}

/// Replacement when the summarizer failed after a retry: structural drop
/// with an honest marker (degrade lossy, never wedge).
pub fn drop_marker_message() -> Message {
    Message::user("[Earlier conversation truncated — summary unavailable]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> CompactionPolicy {
        CompactionPolicy {
            high_water: 0.75,
            low_water: 0.50,
            context_window: 1000,
            output_reservation: 200,
            summary_model: None,
            summary_prompt: None,
        }
    }

    fn text_of_len(n: usize) -> String {
        "x".repeat(n)
    }

    /// A complete tool-using turn: user text, assistant tool call, tool
    /// result, assistant answer.
    fn tool_turn(user: &str, result_len: usize) -> Vec<Message> {
        vec![
            Message::user(user),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "f.rs"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: text_of_len(result_len),
                    is_error: false,
                }],
            },
            Message::assistant("done"),
        ]
    }

    // --- policy math ---

    #[test]
    fn budget_subtracts_output_reservation() {
        assert_eq!(policy().budget(), 800);
    }

    #[test]
    fn budget_saturates_when_reservation_exceeds_window() {
        let p = CompactionPolicy {
            output_reservation: 2000,
            ..policy()
        };
        assert_eq!(p.budget(), 0);
    }

    #[test]
    fn should_compact_at_and_above_high_water() {
        let p = policy(); // high water = 0.75 * 800 = 600
        assert!(!p.should_compact(599));
        assert!(p.should_compact(600));
        assert!(p.should_compact(601));
    }

    #[test]
    fn target_is_low_water_fraction_of_budget() {
        assert_eq!(policy().target_tokens(), 400); // 0.50 * 800
    }

    // --- estimator ---

    #[test]
    fn estimate_counts_all_block_kinds() {
        let msgs = vec![
            Message::user(text_of_len(400)), // 100 tokens
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking(text_of_len(200))], // 50 tokens
            },
        ];
        assert_eq!(estimate_tokens(&msgs), 150);
    }

    #[test]
    fn image_prompt_starts_turn_and_summary_omits_base64_payload() {
        let message = Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                data: "sensitive-base64-payload".into(),
                media_type: "image/png".into(),
                uri: Some("file:///workspace/screenshot.png".into()),
            }],
        };

        assert_eq!(turn_starts(std::slice::from_ref(&message)), vec![0]);
        let transcript = transcript_for_summary(&[message]);
        assert!(transcript.contains("image/png"));
        assert!(transcript.contains("file:///workspace/screenshot.png"));
        assert!(!transcript.contains("sensitive-base64-payload"));
    }

    #[test]
    fn estimate_prompt_includes_system() {
        let msgs = vec![Message::user(text_of_len(400))]; // 100
        assert_eq!(estimate_prompt_tokens(Some(&text_of_len(200)), &msgs), 150);
        assert_eq!(estimate_prompt_tokens(None, &msgs), 100);
    }

    // --- turn segmentation ---

    #[test]
    fn tool_result_messages_do_not_start_turns() {
        let mut msgs = tool_turn("first", 10);
        msgs.extend(tool_turn("second", 10));
        // Two genuine user-text turns at 0 and 4; the ToolResult user
        // messages at 2 and 6 must not count.
        assert_eq!(turn_starts(&msgs), vec![0, 4]);
    }

    // --- choose_cut ---

    #[test]
    fn single_turn_is_never_evicted() {
        let msgs = tool_turn("only", 100_000); // enormous, but the live tail
        assert_eq!(choose_cut(&msgs, 10), None);
    }

    #[test]
    fn cut_never_splits_a_tool_pair() {
        let mut msgs = Vec::new();
        for i in 0..5 {
            msgs.extend(tool_turn(&format!("task {i}"), 4000)); // ~1k tokens/turn
        }
        let cut = choose_cut(&msgs, 2000).expect("must evict something");
        // The cut must land on a genuine user-text turn start…
        assert!(
            turn_starts(&msgs).contains(&cut),
            "cut {cut} not a turn start"
        );
        // …so every ToolCall id in the kept tail has its ToolResult there too.
        let kept = &msgs[cut..];
        let call_ids: Vec<&String> = kept
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| {
                if let ContentBlock::ToolCall { id, .. } = b {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        for id in call_ids {
            assert!(
                kept.iter().flat_map(|m| &m.content).any(|b| matches!(
                    b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                )),
                "kept ToolCall {id} lost its ToolResult"
            );
        }
    }

    #[test]
    fn cut_keeps_the_straddling_turn() {
        // 3 turns of ~1000 tokens each; target 1500 lands inside the middle
        // turn — round outward: keep the middle turn (cut at its start).
        let mut msgs = Vec::new();
        for i in 0..3 {
            msgs.extend(tool_turn(&format!("task {i}"), 4000));
        }
        let starts = turn_starts(&msgs);
        let cut = choose_cut(&msgs, 1500).unwrap();
        assert_eq!(cut, starts[1], "the turn the target lands in must be kept");
    }

    #[test]
    fn history_already_under_target_evicts_nothing() {
        let mut msgs = tool_turn("a", 40);
        msgs.extend(tool_turn("b", 40));
        assert_eq!(choose_cut(&msgs, 10_000), None);
    }

    #[test]
    fn oversized_last_turn_still_keeps_only_last_turn() {
        let mut msgs = tool_turn("old", 400);
        msgs.extend(tool_turn("huge", 100_000)); // last turn alone >> target
        let starts = turn_starts(&msgs);
        let cut = choose_cut(&msgs, 100).unwrap();
        assert_eq!(cut, starts[1], "evict everything but the live tail");
    }

    // --- transcript ---

    #[test]
    fn transcript_labels_roles_and_caps_tool_results() {
        let mut msgs = tool_turn("do the thing", 2000);
        msgs.push(Message::assistant("all done"));
        let t = transcript_for_summary(&msgs);
        assert!(t.contains("User: do the thing"), "{t}");
        assert!(t.contains("Assistant: done"), "{t}");
        assert!(t.contains("[tool call: read_file"), "{t}");
        assert!(t.contains("[tool result: "), "{t}");
        assert!(t.contains('…'), "long tool result must be capped: {t}");
        // Capped at 500 chars of x's, not 2000.
        assert!(!t.contains(&text_of_len(501)), "tool result exceeded cap");
    }

    #[test]
    fn transcript_omits_provider_state_but_estimates_its_size() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ProviderState {
                provider: "openai".into(),
                data: serde_json::json!({"type":"reasoning","encrypted_content":"SECRET_OPAQUE_STATE"}),
            }],
        }];
        assert!(!transcript_for_summary(&messages).contains("SECRET_OPAQUE_STATE"));
        assert!(estimate_tokens(&messages) > 0);
    }

    #[test]
    fn transcript_omits_thinking() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking("secret reasoning".into()),
                ContentBlock::Text("visible".into()),
            ],
        }];
        let t = transcript_for_summary(&msgs);
        assert!(!t.contains("secret reasoning"));
        assert!(t.contains("Assistant: visible"));
    }

    #[test]
    fn transcript_marks_tool_errors() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "boom".into(),
                is_error: true,
            }],
        }];
        assert!(transcript_for_summary(&msgs).contains("[tool error: boom]"));
    }

    // --- replacement messages ---

    #[test]
    fn summary_message_is_user_role_with_marker() {
        let m = summary_message("we fixed the auth bug");
        assert_eq!(m.role, Role::User);
        assert!(matches!(
            &m.content[0],
            ContentBlock::Text(t) if t.starts_with("[Summary of earlier conversation:") && t.contains("auth bug")
        ));
    }

    #[test]
    fn drop_marker_is_user_role() {
        let m = drop_marker_message();
        assert_eq!(m.role, Role::User);
        assert!(
            matches!(&m.content[0], ContentBlock::Text(t) if t.contains("summary unavailable"))
        );
    }

    #[test]
    fn strategy_name_for_logging() {
        assert_eq!(CompactionStrategy::Summarize.as_str(), "summarize");
    }
}

/// Measurement harness for vikunja #1236: how much durable knowledge survives
/// repeated compaction?
///
/// Compaction replaces history with `[summary] + tail` (`agent.rs`), and
/// [`choose_cut`] gives the summary message no protection — it is an ordinary
/// user-role turn start, so the *next* compaction evicts and re-summarizes it.
/// Summaries therefore compound: generation N summarizes generation N-1's
/// summary. This measures whether that actually loses facts, which is the
/// go/no-go evidence for building a durable working-memory store.
#[cfg(test)]
mod memory_loss_1236 {
    use super::*;
    use crate::providers::{ContentBlock, LlmProvider, Message};

    /// Distinct, checkable facts planted early and never repeated. Each is the
    /// kind of thing a durable memory store would exist to retain: a decision,
    /// a location, a constraint.
    /// `(id, statement, needle)`. The needle is stated explicitly rather than
    /// derived: an earlier version guessed it heuristically and, for the rate
    /// limit, found no distinctive token — so it silently fell back to matching
    /// the whole sentence verbatim and scored a surviving fact as lost. A
    /// faithful summary is allowed to reword, so the needle must be the one
    /// token a correct summary cannot paraphrase away.
    const FACTS: &[(&str, &str, &str)] = &[
        (
            "build-feature",
            "the build requires the `zeta-codec` feature flag",
            "zeta-codec",
        ),
        (
            "failing-test",
            "the failing test is at crates/parser/src/lex.rs:412",
            "lex.rs:412",
        ),
        (
            "decision",
            "we chose the ring-buffer approach over a channel because of backpressure",
            "ring-buffer",
        ),
        (
            "constraint",
            "the API rate limit is 40 requests per minute (the RPM_CEILING_40 budget)",
            "rpm_ceiling_40",
        ),
        (
            "gotcha",
            "tests must run with --test-threads=1 or the fixture races",
            "--test-threads=1",
        ),
    ];

    fn noise_turn(i: usize) -> Vec<Message> {
        vec![
            Message::user(format!(
                "Please look at module {i} and describe what it does. {}",
                "Some additional detail about unrelated refactoring work. ".repeat(12)
            )),
            Message::assistant(format!(
                "Module {i} handles routing. {}",
                "It delegates to helpers and returns a result struct. ".repeat(12)
            )),
        ]
    }

    /// Build a history whose first turns establish every fact, followed by
    /// `noise` turns of unrelated work.
    fn seeded_history(noise: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        for (id, text, _) in FACTS {
            messages.push(Message::user(format!("Note this: {text}")));
            messages.push(Message::assistant(format!(
                "Understood — recorded that {text} (ref {id})."
            )));
        }
        for i in 0..noise {
            messages.extend(noise_turn(i));
        }
        messages
    }

    /// Ask the real summarizer to compact `messages`, returning the new history.
    async fn compact_once(
        provider: &dyn LlmProvider,
        model: &str,
        messages: Vec<Message>,
        target_tokens: u64,
    ) -> (Vec<Message>, usize) {
        let Some(cut) = choose_cut(&messages, target_tokens) else {
            return (messages, 0);
        };
        let transcript = transcript_for_summary(&messages[..cut]);
        let response = provider
            .complete(
                &crate::providers::Context {
                    messages: vec![Message::user(format!(
                        "{}\n\n{transcript}",
                        default_summary_prompt()
                    ))],
                    system: None,
                    tools: vec![],
                    stable_prefix_len: 0,
                },
                &crate::providers::CompleteOpts {
                    model: model.to_string(),
                    thinking: crate::providers::ThinkingLevel::Off,
                    temperature: Some(0.0),
                    ..crate::providers::CompleteOpts::default()
                },
            )
            .await;
        let text: String = response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.trim().is_empty(),
            "summarizer returned nothing: {:?}",
            response.error_message
        );
        let mut out = vec![summary_message(&text)];
        out.extend_from_slice(&messages[cut..]);
        (out, cut)
    }

    fn surviving_facts(messages: &[Message]) -> Vec<&'static str> {
        let haystack = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.to_lowercase()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        FACTS
            .iter()
            .filter(|(_, _, needle)| haystack.contains(&needle.to_lowercase()))
            .map(|(id, _, _)| *id)
            .collect()
    }

    #[tokio::test]
    #[ignore = "spends real API tokens on summarization; run with --ignored (vikunja #1236)"]
    async fn measure_fact_survival_across_compaction_generations() {
        let key = std::env::var("DAIMONOS_AGENT_API_KEY")
            .expect("set DAIMONOS_AGENT_API_KEY (see ~/.config/daimonos/agent.env)");
        let model = std::env::var("DAIMONOS_MEMORY_PROBE_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        let provider = crate::providers::anthropic::AnthropicProvider::new(key);

        let generations: usize = std::env::var("DAIMONOS_MEMORY_PROBE_GENERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);

        let mut history = seeded_history(30);
        println!(
            "\n=== #1236 fact survival under repeated compaction (model {model}) ===\ngen 0: {} messages, ~{} est tokens, facts present: {:?}",
            history.len(),
            estimate_tokens(&history),
            surviving_facts(&history)
        );

        for gen in 1..=generations {
            // Aggressive: keep roughly a quarter of the current history.
            let target = (estimate_tokens(&history) / 4).max(200);
            let (next, cut) = compact_once(&provider, &model, history, target).await;
            history = next;
            let alive = surviving_facts(&history);
            println!(
                "gen {gen}: evicted {cut} msgs -> {} messages, ~{} est tokens, {}/{} facts survive: {:?}",
                history.len(),
                estimate_tokens(&history),
                alive.len(),
                FACTS.len(),
                alive
            );
            // Grow the conversation again so the next generation has something
            // new to compact, mirroring a real long-running session.
            for i in 0..20 {
                history.extend(noise_turn(1000 + gen * 100 + i));
            }
        }

        let survivors = surviving_facts(&history);
        println!(
            "\nRESULT: {}/{} planted facts survived {generations} compaction generations: {:?}\n",
            survivors.len(),
            FACTS.len(),
            survivors
        );
    }
}

/// Stress variants for #1236: the two loss channels the primary measurement
/// does not exercise — scale (many competing facts) and burial (a fact that
/// exists only inside a long tool result, where [`SUMMARY_TOOL_RESULT_CAP`]
/// truncates it out of the summarizer's input before the model ever sees it).
#[cfg(test)]
mod memory_loss_stress_1236 {
    use super::*;
    use crate::providers::{ContentBlock, LlmProvider, Message};

    fn provider_and_model() -> (crate::providers::anthropic::AnthropicProvider, String) {
        let key = std::env::var("DAIMONOS_AGENT_API_KEY").expect("DAIMONOS_AGENT_API_KEY");
        let model = std::env::var("DAIMONOS_MEMORY_PROBE_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        (
            crate::providers::anthropic::AnthropicProvider::new(key),
            model,
        )
    }

    async fn summarize(
        provider: &dyn LlmProvider,
        model: &str,
        messages: &[Message],
        cut: usize,
    ) -> String {
        let transcript = transcript_for_summary(&messages[..cut]);
        let response = provider
            .complete(
                &crate::providers::Context {
                    messages: vec![Message::user(format!(
                        "{}\n\n{transcript}",
                        default_summary_prompt()
                    ))],
                    system: None,
                    tools: vec![],
                    stable_prefix_len: 0,
                },
                &crate::providers::CompleteOpts {
                    model: model.to_string(),
                    thinking: crate::providers::ThinkingLevel::Off,
                    temperature: Some(0.0),
                    ..crate::providers::CompleteOpts::default()
                },
            )
            .await;
        response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Does the summarizer still retain facts when many compete for space?
    #[tokio::test]
    #[ignore = "spends real API tokens; run with --ignored (vikunja #1236)"]
    async fn measure_fact_survival_at_scale() {
        let (provider, model) = provider_and_model();
        let n: usize = std::env::var("DAIMONOS_MEMORY_PROBE_FACTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40);

        let mut messages = Vec::new();
        for i in 0..n {
            messages.push(Message::user(format!(
                "Note this: subsystem {i} uses the KEYTOKEN_{i:03} adapter."
            )));
            messages.push(Message::assistant(format!(
                "Understood — subsystem {i} uses KEYTOKEN_{i:03}."
            )));
        }
        for i in 0..40 {
            messages.push(Message::user(format!(
                "Unrelated question {i}. {}",
                "Context padding about refactoring. ".repeat(14)
            )));
            messages.push(Message::assistant(format!(
                "Unrelated answer {i}. {}",
                "It delegates to helpers and returns a struct. ".repeat(14)
            )));
        }

        let cut = choose_cut(&messages, estimate_tokens(&messages) / 5).expect("cut");
        let summary = summarize(&provider, &model, &messages, cut).await;
        let lower = summary.to_lowercase();
        let survived = (0..n)
            .filter(|i| lower.contains(&format!("keytoken_{i:03}")))
            .count();
        println!(
            "\n=== #1236 scale: {survived}/{n} distinct facts retained in one summary \
             (evicted {cut} msgs, summary {} chars) ===\n",
            summary.len()
        );
    }

    /// A fact that exists ONLY inside a long tool result. `transcript_for_summary`
    /// caps each tool result at `SUMMARY_TOOL_RESULT_CAP` (500) chars, so a fact
    /// past that cap never reaches the summarizer at all — a deterministic loss
    /// channel that no amount of summarizer quality can recover.
    #[tokio::test]
    #[ignore = "spends real API tokens; run with --ignored (vikunja #1236)"]
    async fn measure_fact_buried_in_a_long_tool_result() {
        let (provider, model) = provider_and_model();

        // The needle sits well past the 500-char cap.
        let buried = format!(
            "{}\nCRITICAL: the deploy key lives at BURIED_TOKEN_XYZ\n{}",
            "routine log line\n".repeat(80),
            "more routine output\n".repeat(80)
        );
        let mut messages = vec![
            Message::user("Run the diagnostic and tell me what you find."),
            Message {
                role: crate::providers::Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({"cmd": "diagnose"}),
                }],
            },
            Message {
                role: crate::providers::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: buried.clone(),
                    is_error: false,
                }],
            },
        ];
        for i in 0..40 {
            messages.push(Message::user(format!(
                "Next task {i}. {}",
                "Padding. ".repeat(20)
            )));
            messages.push(Message::assistant(format!(
                "Done {i}. {}",
                "More padding. ".repeat(20)
            )));
        }

        let cut = choose_cut(&messages, estimate_tokens(&messages) / 5).expect("cut");
        let transcript = transcript_for_summary(&messages[..cut]);
        let reached_summarizer = transcript.contains("BURIED_TOKEN_XYZ");
        let summary = summarize(&provider, &model, &messages, cut).await;
        let in_summary = summary.contains("BURIED_TOKEN_XYZ");
        println!(
            "\n=== #1236 burial: fact reached summarizer input: {reached_summarizer}; \
             present in summary: {in_summary} (tool-result cap = {SUMMARY_TOOL_RESULT_CAP} chars) ===\n"
        );
    }
}

/// Is the #1236 "lost knowledge" problem actually a *prompt* problem?
///
/// The scale measurement showed 2/40 distinct facts retained, in a 323-char
/// summary of 142 evicted messages. `prompts/summary.md` says "be dense and
/// factual" but gives no completeness requirement and no signal that output
/// length should scale with the material — so the model is free to write three
/// sentences regardless of how much there was to keep.
///
/// This compares the shipped prompt against one that states the completeness
/// obligation explicitly, on byte-identical input. If the gap is large, #1236's
/// premise is a prompt defect, not a missing subsystem.
#[cfg(test)]
mod summary_prompt_ab_1236 {
    use super::*;
    use crate::providers::{ContentBlock, LlmProvider, Message};

    pub(super) const COMPLETENESS_PROMPT: &str = "You are summarizing the earlier part of a conversation between a user and a coding agent so the conversation can continue with the summary in place of the original messages. \
Preserve: the user's overall goal; key decisions and their rationale; files, commands, and resources touched and their current state; important facts learned from tool results; and open threads or next steps. \
COMPLETENESS IS THE PRIMARY REQUIREMENT: every distinct concrete fact — identifiers, paths, versions, flags, numeric limits, decisions — must appear in the summary. \
Losing one is a failure; being long is not. Scale your length to the material: if the conversation established fifty facts, the summary must contain fifty facts. \
Use a terse bulleted list, one fact per line, rather than prose. \
Drop verbatim file contents, command output, and pleasantries. Reply with the summary only.";

    #[tokio::test]
    #[ignore = "spends real API tokens; run with --ignored (vikunja #1236)"]
    async fn compare_shipped_prompt_against_a_completeness_prompt() {
        let key = std::env::var("DAIMONOS_AGENT_API_KEY").expect("DAIMONOS_AGENT_API_KEY");
        let model = std::env::var("DAIMONOS_MEMORY_PROBE_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        let provider = crate::providers::anthropic::AnthropicProvider::new(key);
        let n: usize = 40;

        let mut messages = Vec::new();
        for i in 0..n {
            messages.push(Message::user(format!(
                "Note this: subsystem {i} uses the KEYTOKEN_{i:03} adapter."
            )));
            messages.push(Message::assistant(format!(
                "Understood — subsystem {i} uses KEYTOKEN_{i:03}."
            )));
        }
        for i in 0..40 {
            messages.push(Message::user(format!(
                "Unrelated question {i}. {}",
                "Context padding about refactoring. ".repeat(14)
            )));
            messages.push(Message::assistant(format!(
                "Unrelated answer {i}. {}",
                "It delegates to helpers and returns a struct. ".repeat(14)
            )));
        }
        let cut = choose_cut(&messages, estimate_tokens(&messages) / 5).expect("cut");
        let transcript = transcript_for_summary(&messages[..cut]);

        for (label, prompt) in [
            ("shipped ", default_summary_prompt()),
            ("complete", COMPLETENESS_PROMPT.to_string()),
        ] {
            let response = provider
                .complete(
                    &crate::providers::Context {
                        messages: vec![Message::user(format!("{prompt}\n\n{transcript}"))],
                        system: None,
                        tools: vec![],
                        stable_prefix_len: 0,
                    },
                    &crate::providers::CompleteOpts {
                        model: model.clone(),
                        thinking: crate::providers::ThinkingLevel::Off,
                        temperature: Some(0.0),
                        // Generous: the point is to see how much the model
                        // *chooses* to write, not to cap it.
                        max_tokens: 8192,
                    },
                )
                .await;
            let text: String = response
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            let kept = (0..n)
                .filter(|i| text.contains(&format!("keytoken_{i:03}")))
                .count();
            println!(
                "=== #1236 A/B [{label}] retained {kept}/{n} facts in {} chars ({} est tokens) ===",
                text.len(),
                text.len() / 4
            );
        }
    }
}

/// Realistic-material A/B for #1236.
///
/// The synthetic A/B used `KEYTOKEN_NNN` markers, which are unusually easy for a
/// model to enumerate — a summarizer can pattern-match and list them without
/// understanding anything. That risks overstating the fix. This repeats the
/// comparison on facts phrased the way they actually arise in a coding session:
/// embedded in explanation and tool output, never announced as "note this", and
/// with needles that a faithful summary cannot paraphrase away (identifiers,
/// paths, versions, numbers).
#[cfg(test)]
mod realistic_retention_1236 {
    use super::*;
    use crate::providers::{ContentBlock, LlmProvider, Message, Role};

    /// `(needle, user_turn, assistant_turn)` — facts emerge from ordinary
    /// dialogue rather than being dictated.
    const EXCHANGES: &[(&str, &str, &str)] = &[
        ("tokio-console",
         "The async tasks are stalling somewhere and I can't tell where.",
         "I compared the options and went with tokio-console over tracing-flame here — tracing-flame needs a full rebuild with a custom subscriber, and tokio-console attaches to the running process, which matters because the stall only shows up under real load."),
        ("crates/ingest/src/batch.rs",
         "Where is the batching actually happening?",
         "It's in crates/ingest/src/batch.rs — the flush path there collects up to the window size before handing off. That file is the one to change if you want different batching behaviour."),
        ("0.14.2",
         "Did you pin the dependency?",
         "Yes — I pinned rdkafka to 0.14.2 rather than taking 0.15, because 0.15 changed the consumer rebalance callback signature and our shutdown path depends on the old behaviour."),
        ("512",
         "What did we settle on for the queue?",
         "We settled on a bounded queue of 512 entries. Unbounded caused the OOM you saw on Tuesday, and anything under about 256 made the producer block during normal bursts."),
        ("--no-default-features",
         "The CI build keeps failing but it works locally.",
         "CI builds with --no-default-features, which drops the `compat` shim that your local build picks up by default. That's why the symbol resolves for you and not in CI."),
        ("PG_STATEMENT_TIMEOUT",
         "Queries are dying after a while in staging.",
         "Staging sets PG_STATEMENT_TIMEOUT to 30s at the connection-pool level, so anything longer gets killed server-side regardless of what the client asks for."),
        ("read-through",
         "How should the cache behave on a miss?",
         "We decided on a read-through cache rather than cache-aside, because cache-aside let two workers both miss and both hit the database for the same key during the morning spike."),
        ("legacy_ids",
         "Anything else I should know before touching the migration?",
         "The legacy_ids table is still referenced by the reporting job even though nothing writes to it any more. Dropping it will break the nightly report, so it has to outlive this migration."),
    ];

    fn realistic_history() -> Vec<Message> {
        let mut messages = Vec::new();
        for (_, user, assistant) in EXCHANGES {
            messages.push(Message::user((*user).to_string()));
            messages.push(Message::assistant((*assistant).to_string()));
        }
        // Ordinary follow-on work that pushes the above out of the window.
        for i in 0..45 {
            messages.push(Message::user(format!(
                "Now rename the helper in module {i} and run the tests."
            )));
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text(format!(
                    "Renamed it in module {i} and the suite passes. {}",
                    "The change was mechanical: update the definition, the two call sites, and the doc comment. ".repeat(6)
                ))],
            });
        }
        messages
    }

    async fn retention(
        provider: &dyn LlmProvider,
        model: &str,
        transcript: &str,
        prompt: &str,
    ) -> (usize, usize) {
        let response = provider
            .complete(
                &crate::providers::Context {
                    messages: vec![Message::user(format!("{prompt}\n\n{transcript}"))],
                    system: None,
                    tools: vec![],
                    stable_prefix_len: 0,
                },
                &crate::providers::CompleteOpts {
                    model: model.to_string(),
                    thinking: crate::providers::ThinkingLevel::Off,
                    temperature: Some(0.0),
                    max_tokens: 8192,
                },
            )
            .await;
        let text = response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        let kept = EXCHANGES
            .iter()
            .filter(|(needle, _, _)| text.contains(&needle.to_lowercase()))
            .count();
        (kept, text.len())
    }

    #[tokio::test]
    #[ignore = "spends real API tokens; run with --ignored (vikunja #1236)"]
    async fn realistic_facts_shipped_prompt_versus_completeness_prompt() {
        let key = std::env::var("DAIMONOS_AGENT_API_KEY").expect("DAIMONOS_AGENT_API_KEY");
        let model = std::env::var("DAIMONOS_MEMORY_PROBE_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        let provider = crate::providers::anthropic::AnthropicProvider::new(key);

        let messages = realistic_history();
        let cut = choose_cut(&messages, estimate_tokens(&messages) / 5).expect("cut");
        let transcript = transcript_for_summary(&messages[..cut]);
        let n = EXCHANGES.len();
        println!(
            "\n=== #1236 realistic A/B: {n} facts, evicted {cut} of {} messages ===",
            messages.len()
        );

        for (label, prompt) in [
            ("shipped ", default_summary_prompt()),
            (
                "complete",
                super::summary_prompt_ab_1236::COMPLETENESS_PROMPT.to_string(),
            ),
        ] {
            let (kept, chars) = retention(&provider, &model, &transcript, &prompt).await;
            println!(
                "[{label}] retained {kept}/{n} facts in {chars} chars (~{} tokens)",
                chars / 4
            );
        }
        println!();
    }
}
