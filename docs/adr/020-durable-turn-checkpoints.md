# ADR-020: Persist recoverable turn checkpoints before the next action

- **Status:** Proposed
- **Date:** 2026-10-28
- **Deciders:** Not recorded. The user authorized the repair, not ADR acceptance.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** [Local investigation and repair scope](../investigations/2026-10-28-acp-crash-history.md)
- **Supersedes:** None. Extends ADR-014, ADR-015, and ADR-016.
- **Superseded by:** None

## Context

The configured ACP binary loses an active turn after SIGKILL, SIGTERM, or stdin EOF.
The synthetic probe confirmed completed tool results before terminating each active process.
Previously completed prompts survived the control cases.
The desktop logout/OOM investigation is separate.

ADR-016 keeps a cancellation-surviving journal in memory. The live canonical history changes only at finalization.
ACP teardown signals cancellation but previously returned before the terminal persistence write.
SQLite already provides transactional writes, writer epochs, and increasing capture generations.

How can Daimonos preserve authoritative active-turn progress without an additional store or early in-memory commits?

## Decision drivers

- Preserve accepted prompts, completed provider messages, and completed tool results.
- Exclude unfinished streamed text from model history.
- Keep normal in-memory turns transactional and preserve retry replacement semantics.
- Reuse writer fencing, ordering, interruption materialization, and frontend recovery.
- Avoid unbounded queues and per-token writes during an OOM investigation.
- Keep shutdown bounded when an MCP bridge or storage operation stalls.

## Decision

We will persist a recovery projection through the existing session record at authoritative boundaries.
The live AgentSession history remains staged until its existing terminal commit.
The on-disk checkpoint represents the history that a new process can safely resume after interruption.
The terminal save replaces that checkpoint with the final completed or cancelled history.

1. SessionCore owns persistence metadata, writer admission, and generation allocation.
2. AgentSession supplies authoritative message snapshots through an awaited checkpoint hook.
3. Persist the accepted prompt before publishing its canonical user event or calling the provider.
4. Persist complete provider messages before executing their requested tools.
5. Persist completed tool results before advancing to another tool or provider generation.
6. Use ADR-016 interruption materialization to close unmatched calls and add the existing cancellation marker.
7. Record an aborted outcome in recovery projections. The normal terminal save records the actual outcome.
8. Preserve client message IDs. Explicit retries replace their existing outcome rather than adding a user turn.
9. Await each blocking SQLite save through Tokio's existing blocking pool. Do not create an independent queue or callback thread.
10. Keep checkpoint durability status unsaved while streamed or future turn work remains. Report save failures as degraded.
11. Share the core generation counter with checkpoint captures. Existing epoch checks reject superseded writers.
12. ACP shutdown waits for active-turn permit release and bridge shutdown concurrently under the existing teardown deadline.

The persistence format and database schema do not change.
Older binaries can read the recovery projection as interrupted history.
Partial streamed chunks remain transient. A completed provider response remains authoritative even when its stop reason reports an error.

## Alternatives

- **Drain shutdown only:** cannot protect against SIGKILL or OOM termination.
- **Save only the user prompt:** loses completed work from long tool-running turns.
- **Periodic snapshots:** leave a timer-sized loss window and need another lifecycle worker.
- **Separate checkpoint table:** duplicates writer fencing and recovery admission without a present need.
- **Append a new disk journal:** needs replay, truncation, migration, and cross-store consistency rules.
- **Commit live history at each tool boundary:** changes the transactional semantics that ADR-016 preserves.
- **Save every streamed chunk:** persists provider-invalid fragments and increases storage pressure.

## Consequences

One bounded, awaited write occurs at each authoritative boundary. This adds storage latency.
Repeated full snapshots increase serialization work and SQLite write traffic during long turns.
There is no unbounded queue of snapshots or additional background worker.
The session catalog can show an interrupted recovery projection while its live turn still runs.
A save failure remains visible. This change cannot guarantee durability when storage cannot commit.
A process can still die between a tool's external side effect and its checkpoint commit.
Recovery must not claim that an unmatched call produced no side effects.

## Confirmation

- A teardown test fails before the drain and passes after it.
- The process-level ACP suite preserves authoritative markers after SIGKILL, SIGTERM, and EOF.
- Explicit cancellation and normal completion continue to preserve prior history.
- Checkpoint saves do not mutate live canonical history.
- Unfinished stream text does not appear in restored model history.
- A restarted session can issue another prompt without orphan tool-call errors.
- Retry checkpoints preserve the user-turn count and replace the retried outcome.
- Delayed older captures and superseded writers cannot overwrite newer checkpoints.
- Save failure tests expose degraded durability rather than falsely reporting saved state.
- Run `cargo test -j 1`, focused ACP process tests, and the MCP regression suite.

## Anchors

_Anchors:_ src/agent.rs::TurnJournalWriter, src/agent.rs::AgentSession::staged_prompt_message, src/agent.rs::materialize_cancelled_suffix, src/session_core.rs::SessionCore::prompt_with_active_turn, src/session_core.rs::SessionPersistence::save, src/session_core.rs::TurnController, src/acp_cmd.rs::shutdown_session_handles_with, src/session_store.rs::SessionStore::write_record

## Revisit when

- Checkpoint latency materially slows tool-running turns in measured traces.
- Full-snapshot serialization causes measured memory or storage pressure.
- A second durable store or richer recovery state becomes necessary.

## Open questions

- ADR acceptance remains pending. This is personal work with user-authorized implementation, not an Alix acceptance gate.
- Physical power-loss behavior needs a separate controlled test. Do not induce OOM in this repair.
