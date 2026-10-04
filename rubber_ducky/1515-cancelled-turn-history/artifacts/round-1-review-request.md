**Objective:** Make cancelled agent turns durable and provider-valid so subsequent prompts remember authorization, visible progress, completed tool results, background handles, and uncertain side effects instead of reverting to the prior completed turn.

**Context:** Confirmed production sequence: Zed cancels an active ACP turn before a follow-up; Daimonos drops AgentSession::prompt_message, whose cloned history commits only on normal return. The reported 563-second turn was logged Cancelled, the follow-up started 5.8 ms later, and persisted history jumped from the Slack draft directly to `are you stuck?`. Compaction and stale binaries were ruled out. External side effects are not rolled back. Relevant files are src/agent.rs, src/session_core.rs, src/acp_cmd.rs, src/chat_cmd.rs, src/session_protocol.rs, session replay/rendering consumers, prompts resources, and inline Rust/pytest lifecycle tests.

**Steps:**
- P1: Write ADR 016 defining the cancelled-turn partial-commit boundary and add failing scripted-provider regression tests for the production sequence before implementation.
  - Rationale: The change replaces the current documented all-or-nothing cancellation contract and needs explicit invariants plus TDD coverage.
  - Acceptance: A Zed-style second prompt deterministically cancels a first turn after a harmless completed tool and proves the next provider context currently lacks that turn.
  - Acceptance: Tests cover cancel before first response, after a completed tool, during a tool, explicit retry, and a completed background-job result.
- P2: Add a private AgentSession in-flight turn journal and an internal journal-aware run path while leaving the public one-shot run API unchanged.
  - Rationale: The journal survives future drop without making partially valid history canonical during normal execution.
  - Acceptance: The initial user message is journaled before awaiting the provider.
  - Acceptance: Each safe provider/tool boundary updates a bounded snapshot and completed usage.
  - Acceptance: Normal completion uses the existing authoritative AgentResult commit and clears the journal.
- P3: Materialize cancelled history into a provider-valid sequence, retaining authoritative completed blocks and closing unresolved ToolCalls with an explicit uncertain-side-effects ToolResult.
  - Rationale: Structured tool history prevents both provider rejection and accidental repetition of work whose outcome may be external and non-transactional.
  - Acceptance: Every ToolCall has exactly one adjacent ToolResult after finalization.
  - Acceptance: Completed bounded results, including bg handles, remain exact.
  - Acceptance: Unfinished calls never claim they changed nothing; the next model is instructed to inspect state before retrying.
  - Acceptance: Partial stream handling is deterministic and valid for all providers.
- P4: Finalize cancellation atomically in SessionCore: reconcile history, usage, client message IDs, assistant outcome, tool cleanup, durability, and persistence before releasing the active-turn permit.
  - Rationale: Zed waits for the cancelled send task before issuing the follow-up, so this ordering guarantees the next prompt reads the recovered turn.
  - Acceptance: A new-turn cancellation adds one client ID and one cancelled outcome; retry cancellation replaces rather than appends.
  - Acceptance: Late cancellation cannot override a completed turn.
  - Acceptance: Cross-process load restores the same provider history and aligned metadata.
- P5: Route all frontends through the core semantics and update replay/rendering/protocol consumers, including ACP, chat Ctrl-C, session daemon/TUI, and Android if a Cancelled outcome is added.
  - Rationale: History reconstruction must remain frontend-neutral and clients must not diverge or duplicate already rendered entries.
  - Acceptance: Live ACP emits no duplicate text/tool entries when finalizing.
  - Acceptance: Session load replays the cancelled turn once with correct tool terminal states.
  - Acceptance: Chat Ctrl-C persists the same recovered history.
  - Acceptance: All Rust and Android outcome matches remain exhaustive.
- P6: Externalize the cancellation/uncertain-side-effect message through the prompt resource and configuration system, with embedded default, runtime override, dump/print support, and documentation.
  - Rationale: Repository policy forbids new hardcoded model-facing text.
  - Acceptance: No new model-facing cancellation prose is inline in Rust.
  - Acceptance: Default and override tests prove the exact text reaches synthesized ToolResults/history.
- P7: Run focused and full validation, then perform a harmless live Zed ACP cancellation check and record delivery on task 1515.
  - Rationale: The bug crosses async lifecycle, persistence, provider shape, and a real ACP client; unit coverage alone is insufficient.
  - Acceptance: Focused agent/session/ACP tests pass; cargo fmt, clippy -D warnings, cargo test, and full pytest pass.
  - Acceptance: A release binary is installed and a restarted Zed session retains a harmless cancelled turn in both the next provider context and persisted session.
  - Acceptance: Task 1515 records ADR, tests, commit/PR, and live evidence before closure.

**Acceptance criteria:**
- The follow-up provider request contains the cancelled user authorization exactly once.
- Every completed tool call and bounded result from the cancelled turn survives, including background-job handles.
- Every unresolved call is provider-valid and explicitly records uncertain side effects; no path says it changed nothing.
- Cancellation history, client IDs, outcomes, usage, persistence, replay, and live UI remain aligned.
- Normal completed turns, context-overflow retry, explicit retry, compaction, max-token continuation, approval cleanup, and exactly-once tool terminal updates do not regress.
- ACP and chat use one core cancellation implementation; no frontend reconstructs model history.
- All repository validation and a harmless live Zed check pass.

**Risks:**
- Repeated full-history journal snapshots may add bounded but nontrivial clone cost on tool-heavy turns.
- Preserving partial streamed text without provider-owned continuation state can corrupt replay if treated as authoritative.
- Adding a wire outcome variant requires coordinated Android/TUI compatibility work.
- Cancellation may arrive during analytics draining or post-provider bookkeeping, so journal completion and TurnSignal claims must share a clear winner.
- Synthetic results for multi-call responses must preserve provider-specific call/result adjacency and opaque ProviderState ordering.