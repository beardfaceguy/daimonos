# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="vikunja-1515-cancelled-turn-code-final" -->

<!-- event id="request" artifact path="vikunja-1515-cancelled-turn-code-final-vikunja-1515-cancelled-turn-code-final/artifacts/round-1-review-request.diff" sha256="fcadee4760be3e13da32c5a4a8458ca173ce944e03b821bccf16899114d19b7f" -->
## Review Request — Round 1
**Task:** vikunja-1515-cancelled-turn-code-final — Final code review: preserve authoritative cancelled-turn state
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Implement ADR-016 on current master (commit 639caca). Normal turns remain transactional. Cancellable ACP/chat paths start an AgentSession turn-suffix journal synchronously, run a staged attempt, and commit only after the existing TurnSignal completion claim wins. Cancellation finalizes the journal instead: append/replace the exact external user message, complete provider-authored messages and ProviderState, every bounded authoritative ToolResult, synthetic error results for unresolved calls, completed usage, and a model-facing safety note from the new overridable cancelled_turn prompt. SessionCore aligns IDs/outcomes and persists before releasing the active-turn permit. Explicit retry replaces its original turn and outcome. Unfinalized stream text remains live-UI-only. Anthropic/OpenAI/OpenRouter request-shape tests cover multi-call completed/unresolved histories.

### Relevant Code / Diff
Commit 639caca changes 17 files (+1566/-79): agent loop/session lifecycle, ACP/chat/session persistence, all three provider tests, prompt config/docs, ADR-016.

Core staged API:
```
pub(crate) fn staged_prompt_message(&mut self, user_message: Message) -> Pin<Box<dyn Future<Output=AgentResult> + Send + '_>> {
    self.begin_turn_journal(TurnJournalMode::Append, user_message.clone());
    Box::pin(async move { self.run_prompt_attempt(user_message).await })
}
pub(crate) fn commit_staged(&mut self, result: AgentResult, completion_claimed: bool) -> TurnResult {
    debug_assert!(completion_claimed);
    self.inflight_turn = None;
    self.commit(result)
}
pub(crate) fn finalize_cancelled_turn(&mut self, usage_already_applied: bool) -> Option<CancelledTurnKind> {
    let journal = self.inflight_turn.take()?;
    let state = journal.lock().unwrap_or_else(|p| p.into_inner());
    let suffix = materialize_cancelled_suffix(state.suffix.clone(), configured_note.trim());
    match state.mode { Append => self.messages.extend(suffix), ReplaceFrom(i) => { self.messages.truncate(i); self.messages.extend(suffix); } }
    if !usage_already_applied { self.total_usage = accumulate_usage(take(&mut self.total_usage), state.usage.clone()); ... }
    Some(kind)
}
```

Single-winner core:
```
let staged_result = { let staged = agent_session.staged_prompt_message(user_message); tokio::select! { result = staged => Some(result), _ = active_turn.cancelled() => None } };
let (turn, cancelled_kind) = match staged_result {
  Some(result) if active_turn.mark_completed() => (Some(agent_session.commit_staged(result, true)), None),
  Some(_) => (None, agent_session.finalize_cancelled_turn(true)),
  None => (None, agent_session.finalize_cancelled_turn(false)),
};
// align IDs/outcomes, persist_current().await, then drop(active_turn)
```

Journal boundaries:
- checkpoint full suffix after each complete provider response and continuation repair
- record cumulative completed-provider usage
- after each bounded tool result, checkpoint assistant call plus partial ordered result batch
- policy-blocked results are checkpointed before `continue`
- final batch checkpoint follows insertion into normal history

Materialization:
```
for each assistant message's ToolCall ids:
  consume an adjacent all-ToolResult user message if present;
  emit results in call order, preserving exact existing blocks;
  synthesize error ToolResult(note) only for missing ids;
append one assistant cancellation note unless empty.
```

Acceptance tests added/updated:
- cancellation before first future poll retains user
- completed write then cancellation retains exact result
- multi-call completed + unresolved pairing
- blocked first call retains exact policy result when second call is cancelled
- unfinalized stream text is not promoted
- completed background-handle-shaped result survives
- staged completion/cancel usage is not double-counted
- normal/cancel journal resources are released
- ACP cancel then follow-up sees recovered context; persisted reload contains it once
- cancelled explicit retry preserves one user/client ID, Aborted outcome, uncertainty result, and persisted record
- Anthropic signed thinking, OpenAI encrypted reasoning state, and OpenRouter multi-call wire shapes each pair every call exactly once
- prompt default/override/dump/config docs coverage.

### Known Concerns
1. The turn journal stores only the current turn suffix but replaces it with a cloned authoritative snapshot at each safe boundary; its lifetime is one turn and provider context/output limits bound it.
2. Chat Ctrl-C directly invokes the same AgentSession finalizer but does not have an injectable OS-signal integration test; AgentSession finalization and ACP lifecycle are covered.
3. A harmless live Zed process check is still pending; deterministic in-process ACP cancel-before-follow-up and persisted reload coverage pass.
4. Full local validation: fmt and all-feature/all-target Clippy clean; 1,889 Rust tests pass with only unavailable ShellCheck tests skipped; Python MCP suite 338 passed, 1 skipped, 1 known master failure excluded.

### Specific Questions for Reviewer
1. Does TurnSignal plus staged commit/finalize prevent every completion-vs-cancel overwrite or double-accounting race?
2. Can materialize_cancelled_suffix ever produce provider-invalid or misordered multi-call history?
3. Are client IDs, AssistantOutcome::Aborted, retry replacement, persistence ordering, and replay aligned?
4. Is any authoritative tool result still vulnerable to cancellation before journaling?
5. Do the added tests satisfy the task and ADR, including provider state, partial streams, blocked tools, retries, and background handles?

<!-- event id="round-1-review-response" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Chat Ctrl-C lacks an injectable OS-signal integration test. Since the finalizer itself is covered via AgentSession and ACP lifecycle tests, this is acceptable, but consider extracting the signal-to-finalizer dispatch behind a testable seam in a follow-up so the chat path's wiring (not just the finalizer) has deterministic coverage.
S2. finalize_cancelled_turn uses lock().unwrap_or_else(|p| p.into_inner()), accepting a potentially poisoned journal snapshot. If a panic can occur mid-checkpoint, document (or assert via an invariant check on the suffix, e.g., call/result pairing validation before materialization) that a torn checkpoint cannot produce provider-invalid history; materialize_cancelled_suffix appears to repair missing results but a comment or debug_assert would make the poisoning contract explicit.
S3. In commit_staged, debug_assert!(completion_claimed) means release builds would silently commit even if a caller passed false. Consider making the parameter unnecessary (encode the claim in the type, e.g., a CompletionClaim token from mark_completed()) or upgrading to a hard error, so the single-winner invariant is enforced structurally rather than by convention.
S4. Complete the pending harmless live Zed process check before release tagging and record its result in the task log; the deterministic in-process ACP coverage is sufficient for approval, but the live check closes the loop on real-client cancel behavior.

**Verdict:** APPROVE
