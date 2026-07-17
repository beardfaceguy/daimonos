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
                // Thinking is internal reasoning; the assistant's visible text
                // restates anything that mattered.
                ContentBlock::Thinking(_) => {}
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
