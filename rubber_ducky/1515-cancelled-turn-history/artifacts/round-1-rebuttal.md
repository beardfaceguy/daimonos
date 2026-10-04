**Objective:** Make cancelled turns durable and provider-valid through a staged attempt/commit protocol and an append-only turn-suffix journal, so later prompts retain authorization, completed tool results, background handles, and truthful uncertainty without changing normal atomic commits.

**Context:** Zed cancels a running ACP turn before a follow-up. Daimonos currently drops a cloned-history prompt future, discarding the user/tool history while side effects survive. The fix must preserve current normal-turn and reactive-overflow behavior, use the existing TurnSignal as the sole race arbiter, and avoid a persisted protocol variant that breaks older readers.

**Steps:**
- P1: Add ADR 016 and deterministic failing tests that freeze the cancellation contract before implementation.
  - Rationale: The ADR resolves the architectural choices up front: staged attempt/commit, TurnSignal single-winner arbitration, authoritative-checkpoint-only history, no grace for in-flight tools, Aborted as the compatible persisted outcome, and UI-only unfinalized stream deltas.
  - Acceptance: Barrier-driven tests reproduce Zed cancel-before-follow-up and both completion/cancel race orderings.
  - Acceptance: Fixtures cover cancel before response, after completed tool, during tool, completed and unresolved bg spawn, explicit retry, and cross-process reload.
- P2: Split AgentSession prompt/retry execution into a staged attempt and explicit commit/finalize API, backed by an append-only in-flight turn-suffix journal.
  - Rationale: A dropped attempt future leaves the journal reachable in AgentSession; callers commit only after winning completion, or materialize it after cancellation. Public non-cancellable prompt helpers retain their current behavior by staging then committing internally.
  - Acceptance: The journal records append-versus-replace turn boundary, initial user message, complete provider Message blocks, completed bounded ToolResults, pending calls, cumulative completed-call usage, and last prompt usage.
  - Acceptance: The existing public `run`/prompt behavior and normal AgentResult commit remain source-compatible where practical.
  - Acceptance: Reactive context-overflow resets the attempt journal to the compacted baseline without committing the failed attempt.
- P3: Journal each tool batch at authoritative boundaries and materialize cancelled history with exact completed results plus synthetic error results for every unresolved call.
  - Rationale: The turn suffix remains provider-valid while accurately distinguishing known results from possible side effects.
  - Acceptance: Each completed result is journaled immediately after output bounding and before the next tool begins.
  - Acceptance: Cancellation does not wait: any uncheckpointed call gets one externalized uncertain-side-effects ToolResult.
  - Acceptance: Multi-call assistant responses retain provider content/ProviderState ordering and exactly one result per call.
  - Acceptance: Checkpointed bg results retain their handle; an uncheckpointed bg/remote/script/foreground call is marked uncertain.
- P4: Use TurnSignal as the sole outcome claim and finalize SessionCore state and persistence under the session lock before releasing the active turn.
  - Rationale: A staged attempt may commit only after `try_mark_completed` wins; cancellation owns every other path and finalization is exactly once.
  - Acceptance: Completion winner commits AgentResult, clears journal, updates usage/outcome/IDs, and persists as today.
  - Acceptance: Cancellation winner materializes the journal, accumulates only journaled usage, aligns one new client ID for a new turn or preserves the existing ID for retry, writes AssistantOutcome::Aborted, persists, then emits terminal Cancelled.
  - Acceptance: A late cancel cannot relabel or overwrite committed history; a late completion cannot overwrite cancelled history.
- P5: Keep frontend behavior thin and update replay tests: ACP/session daemon consume SessionCore state, chat Ctrl-C calls the same finalizer, and no new wire outcome variant is introduced.
  - Rationale: The compatibility mapping avoids Rust/Android decoder changes while preserving canonical cancellation status.
  - Acceptance: Live Zed receives no duplicate history chunks during finalization; already streamed partial deltas remain live-UI-only.
  - Acceptance: Session load replays authoritative cancelled history once and reconstructs unresolved tools as cancelled/failed without orphan calls.
  - Acceptance: Cancelled explicit retry replaces the old turn, preserves its client ID, and does not duplicate the user message after process restart.
  - Acceptance: Chat Ctrl-C persists the same journal finalization semantics.
- P6: Add an embedded and runtime-overridable cancellation prompt resource used for the turn note and unresolved ToolResult, with config/default/docs/print-dump tests.
  - Rationale: New model-facing safety text cannot be hardcoded in Rust.
  - Acceptance: The text states that cancellation may leave side effects and current state must be inspected before retrying or denying changes.
  - Acceptance: Completed calls never receive uncertainty text; unresolved calls always do.
  - Acceptance: No new cancellation prose is inline in operational Rust.
- P7: Validate provider shapes, lifecycle races, full suites, and a harmless live Zed cancellation before delivery.
  - Rationale: The materialized history must satisfy real provider adapters and the actual cancel-before-follow-up client sequence.
  - Acceptance: Anthropic, OpenAI, and OpenRouter serializer/request-shape tests accept journal materialization with multi-call, ProviderState, completed, and unresolved results.
  - Acceptance: Focused cancellation/persistence tests, cargo fmt, clippy -D warnings, cargo test, and full pytest pass.
  - Acceptance: Installed release plus restarted Zed preserves a harmless cancelled tool turn in the next provider context and persisted session; task 1515 records ADR, commit/PR, and evidence.

**Acceptance criteria:**
- TurnSignal provides one tested winner for completion versus cancellation; journal finalization occurs exactly once.
- The next provider request contains the cancelled user request exactly once and every authoritative completed tool result.
- Every uncheckpointed tool call is provider-valid and explicitly uncertain; no path claims it changed nothing.
- Unfinalized stream deltas are never injected into provider history without authoritative provider state.
- New-turn and retry client IDs, Aborted outcomes, usage, persistence, replay, and UI remain aligned.
- Normal turns, reactive overflow, compaction, max-token/error continuation, approval cleanup, and terminal tool updates do not regress.
- No persisted protocol enum is added; older Rust/Android readers remain compatible.
- All provider-shape, Rust, pytest, and harmless live Zed checks pass.

**Risks:**
- Staged attempt/commit is a wider AgentSession API change and must preserve existing direct callers.
- Turn-suffix extraction must remain correct when intra-turn microcompaction mutates earlier tool results.
- Some dropped tool futures may keep external work running, so the uncertainty marker must remain conservative.
- Using Aborted for persisted client cancellation trades outcome precision for compatibility; canonical TurnStatus and telemetry retain the distinction.
- Live UI may show unfinalized text that is intentionally absent after reload; the cancellation note must make that safety tradeoff clear.