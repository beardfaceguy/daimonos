# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="vikunja-1515-cancelled-turn-history-plan" -->

<!-- event id="request" artifact path="1515-cancelled-turn-history/artifacts/round-1-review-request.md" sha256="f29922decba5f31d3a8394fe8c33bd0c572b9e57bf393241f96a33fb22a55625" -->
## Review Request — Round 1
**Task:** 1515 — Preserve cancelled-turn context after tool side effects
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Keep normal AgentSession turns transactional, but add an AgentSession-owned in-flight turn journal whose writer is shared with the internal agent loop. The journal survives when Tokio drops the prompt future. It records the new user message immediately, authoritative provider responses, streamed assistant text, completed bounded tool results, pending tool-call identities, completed usage, and the attempt kind (new turn versus retry). At provider/tool safe boundaries it can materialize a provider-valid cancellation history: completed calls retain exact ToolResults; unanswered calls receive an error ToolResult whose externally configured text says cancellation leaves side effects uncertain and requires state inspection. On cancellation, SessionCore finalizes the journal while holding the AgentSession lock, updates user-message IDs and a new explicit Cancelled assistant outcome, persists before releasing the active-turn permit, then returns ACP Cancelled. Successful turns discard the journal and retain current commit behavior. Chat Ctrl-C calls the same AgentSession finalizer; ACP and session-daemon adapters do not reconstruct history independently.

### Proposed Plan
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

### Known Concerns
1. A shared journal is more machinery than mutating canonical history in place, but preserves current rollback semantics for normal errors/reactive-overflow retries and avoids exposing invalid partial history.
2. ToolUse responses can contain multiple calls and provider continuation state; cancellation materialization must retain actual completed results while pairing every unresolved call without breaking OpenAI/Anthropic ordering.
3. Partial streamed text has no final provider response or opaque ProviderState; preserving user-visible text must not create wire-invalid history.
4. A new AssistantOutcome::Cancelled variant affects Rust session protocol consumers, TUI/frontend rendering, persisted outcomes, and Android sealed-interface decoding.
5. Explicit retry replaces an existing turn rather than appending one; cancellation must replace that turn's outcome/history without adding a client user-message ID.
6. Completed provider usage currently disappears when a prompt future is dropped; the journal should retain completed-call usage without double-counting successful turns.
7. The cancellation text is model-facing and must be an embedded, runtime-overridable prompt resource rather than a new Rust string literal.
8. The completion-versus-cancel claim and persistence ordering must prevent a late cancel from relabeling or overwriting a completed turn.

### Specific Questions for Reviewer
1. Is the shared journal preferable to in-place canonical mutation, or is there a simpler cancellation-safe design that still preserves completed tool results and reactive-overflow semantics?
2. Should persisted outcome alignment add AssistantOutcome::Cancelled, or can an existing outcome/optional slot represent cancellation without semantic or compatibility debt?
3. What is the safest treatment of partial streamed assistant text when opaque provider continuation state is unavailable?
4. Does the plan cover multi-call batches, explicit retry, background jobs, compaction, and cross-process replay without a missed race or duplicate UI entry?

<!-- event id="1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. The completion-versus-cancel race is named as a risk but the plan never specifies the arbitration mechanism. P4 says 'late cancellation cannot override a completed turn' as an acceptance bullet, yet no step defines the single-winner claim: what shared state (TurnSignal, journal state machine, or the AgentSession lock) decides whether the turn is Completed or Cancelled when the provider's final response and the cancel request land concurrently, and which component transitions the journal from 'live' to 'finalized' exactly once. Since finalization happens in SessionCore while the journal writer lives in the internal agent loop, specify the ownership/ordering protocol (e.g., journal is a state machine with a compare-and-swap claim taken before either commit path proceeds) and add a test that races normal commit against cancellation finalization.
B2. P3's treatment of partial streamed assistant text is underspecified despite being listed as a known concern and a question. 'Deterministic and valid for all providers' is not a design decision. The plan must state the concrete rule: is partial streamed text without an authoritative final provider response (a) dropped from provider-facing history entirely and preserved only for UI/persisted rendering, (b) materialized as a plain assistant text message with no ProviderState, or (c) something else? Option semantics differ per provider (Anthropic requires tool_use/tool_result pairing inside assistant/user turns; OpenAI has its own ordering), and an unclear rule here risks wire-invalid history — the exact failure class this task exists to fix. Choose and document the rule in ADR 016 with per-provider validation tests.
B3. Cancellation arriving mid-tool-execution is not addressed. The journal records 'completed bounded tool results' and 'pending tool-call identities', but when Tokio drops the prompt future while a tool is actively running, the tool's side effect may still complete after the drop (spawned subprocess, detached task) or be torn mid-write. The plan's P1 test matrix includes 'during a tool' but P2/P3 never say whether an in-flight tool at drop time is (a) always treated as unresolved/uncertain, (b) given a grace period to complete and journal its real result, or (c) handled differently for background jobs whose handles must survive. Define the drop-time semantics explicitly, including whether tool tasks are structured-concurrency children of the dropped future or detached, since that determines what 'uncertain' truthfully means.
B4. The AssistantOutcome::Cancelled decision is left open (question 2) while five plan steps (P4, P5, persistence, replay, Android decoding) depend on its answer. Persisted-format compatibility is the crux: sessions written by the new binary containing a Cancelled outcome must not break older readers, and Android sealed-interface decoding of an unknown variant is a hard failure mode unless the wire format has an unknown-variant fallback. The plan must resolve this before P2 begins — either commit to the new variant with an explicit forward/backward-compatibility strategy (version gate, tolerant decoding, or default-variant fallback) or choose an existing-outcome-plus-flag encoding, and state which in ADR 016.

### Non-Blocking Suggestions
S1. Answering question 1: the shared journal is preferable to in-place canonical mutation given the requirement to preserve rollback semantics for reactive-overflow retries, but consider a cheaper representation than repeated full-history snapshots — an append-only event log (user msg, provider response, tool result, usage) replayed at finalization would avoid the bounded-but-nontrivial clone cost flagged in the risks and makes the 'safe boundary' concept explicit as event boundaries.
S2. For the explicit-retry case (replacing a turn without adding a client user-message ID), add a dedicated regression test where a retry is cancelled and then the session is loaded cross-process, verifying no duplicate user message and that the replaced turn's original client ID mapping is preserved — this is the path most likely to produce a duplicate UI entry in Zed.
S3. In P6, verify the externalized cancellation text includes the background-job caveat distinctly from ordinary tools: a completed background-job spawn is not 'uncertain' — its handle is known — so consider two message variants (unresolved ordinary call vs. tool cut mid-flight) rather than one blanket uncertainty message, to avoid instructing the model to re-inspect state it already has authoritative results for.
S4. Add an assertion or debug-mode validator that runs the materialized cancellation history through each provider's request-shape validation (call/result adjacency, ProviderState ordering) in tests, rather than relying only on scripted-provider acceptance — this catches future regressions when a provider adds a new response shape.

**Verdict:** REVISE

<!-- event id="2" artifact path="1515-cancelled-turn-history/artifacts/round-1-rebuttal.md" sha256="280babd1d6f09d98dc839692e019593b4485314f2180be006162c06dc2ed5c86" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — Reuse the existing TurnSignal CAS as the sole completion/cancellation authority and split AgentSession execution into staged attempt versus commit. SessionCore selects the attempt against cancellation; if the attempt returns it must win `try_mark_completed()` before committing. If cancellation already owns the signal, SessionCore finalizes the surviving journal instead. The journal itself has no competing outcome CAS, so canonical history, terminal status, and persistence follow one winner. Add a barrier-driven race test for both orderings.
Re B2: ACCEPT — Unfinalized stream deltas are not authoritative and will be excluded from provider-facing and persisted canonical history. They remain visible only in the current live client, which already rendered them. Cancellation materialization uses the last complete provider response/tool checkpoint plus an externalized cancellation note; it never fabricates ProviderState or replays partial thinking/text. ADR 016 will state this rule and Anthropic/OpenAI/OpenRouter serialization tests will validate the materialized shape.
Re B3: ACCEPT — There is no grace period. A tool result is authoritative only after its bounded ToolResult has been appended to the turn journal. Any call without that checkpoint at cancellation is finalized as unresolved with uncertain side effects. Directly awaited foreground futures are dropped; ManagedChild has kill-on-drop, but execute_script spawn_blocking work, remote servers, and a background process whose spawn completed before its result checkpoint may survive, so uncertainty is the only truthful cross-tool rule. A completed `bg` result that was checkpointed retains its exact handle.
Re B4: ACCEPT — Do not add AssistantOutcome::Cancelled. Persist cancellation in the existing AssistantOutcome::Aborted slot while retaining canonical TurnStatus::Cancelled and observability reason `client_cancelled`. Existing Rust/Android readers already decode Aborted and render it as interrupted/aborted, avoiding persisted-wire and downgrade breakage. ADR 016 will document this intentional compatibility mapping.

### Revised Plan
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

### New Points
R1. Adopt S1 as an append-only turn-suffix journal, not repeated full-history snapshots. Canonical pre-turn history remains untouched; the journal stores provider-neutral Message blocks, pending-call state, completed usage, and append-versus-replace metadata. Finalization concatenates or replaces at the recorded turn boundary.
R2. Accept S2 and S4: add cross-process cancelled-retry/client-ID coverage and provider serializer/request-shape validation. Decline S3's second prose variant: completed background spawns retain exact authoritative results and receive no uncertainty text; only uncheckpointed calls receive the single unresolved-call message, so a second message would add complexity without information.

**Requesting:** RE-REVIEW

<!-- event id="3" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. In ADR 016, explicitly document the observable asymmetry created by the B2 resolution: a user who saw streamed partial text in the live client will not see that text after session reload, and the model will never have seen it. Consider having the externalized cancellation note (P6) briefly acknowledge that any partial response was discarded, so the model does not infer it previously answered. The revised risk list mentions this tradeoff; making it a documented invariant with one replay-vs-live rendering test would prevent a future 'bug report' from re-litigating the design.
S6. For the B1 resolution, add a small doc-comment or debug assertion on the staged-attempt commit path stating that commit without a prior successful try_mark_completed() is a protocol violation — the barrier-driven race tests cover today's two callers, but a compile-time-adjacent guard protects future callers of the new staged AgentSession API, which the risks correctly flag as a wider API surface.

### Responses to Prior Points
Re R1: Accepted. The append-only turn-suffix journal is exactly the shape S1 intended: canonical pre-turn history is untouched, safe boundaries become explicit event/checkpoint appends, and finalization is a concatenate-or-replace at a recorded turn boundary. This also cleanly answers the append-versus-replace distinction for explicit retry. The added risk note about intra-turn microcompaction mutating earlier tool results is the right residual to watch — P2's acceptance bullet that reactive overflow resets the journal to the compacted baseline covers it, and the P1 fixtures should exercise that path (they appear to, via the reactive-overflow acceptance in P2 and lifecycle fixtures in P1).
Re R2: Accepted, including the reasoned decline of S3. Your argument is correct: a checkpointed background spawn has an authoritative result and receives no uncertainty text at all, so the distinction S3 asked for is achieved structurally (checkpointed vs. uncheckpointed) rather than through a second prose variant. P6's acceptance bullet 'completed calls never receive uncertainty text; unresolved calls always do' plus P3's bg-handle bullets make this testable. S2 and S4 are properly folded into P5 and P7 acceptance criteria respectively.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
