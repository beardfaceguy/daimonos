# ADR-016: Preserve authoritative cancelled-turn state

- **Status:** Accepted
- **Date:** 2026-09-23
- **Tracking:** Vikunja #1515
- **Anchors:** `src/agent.rs::AgentSession`,
  `src/session_core.rs::SessionCore::prompt_with_active_turn`

## Context

Agent turns historically ran against a clone of canonical message history and
replaced that history only after the complete provider/tool loop returned.
Dropping the future therefore looked cancel-safe: a half-finished turn could
not leave malformed provider history behind.

That transaction covered memory but not the world. A tool may finish, launch a
background process, or mutate an external service before a later cancellation.
Zed sends `session/cancel` before a follow-up prompt when a prior turn is still
active. The old rollback then discarded the user's authorization, assistant
messages, completed tool calls, results, and background handles while keeping
their external effects. The next model request could deny that work happened
or repeat it.

The production incident behind #1515 showed exactly this sequence. A
563-second ACP turn was cancelled immediately before the next prompt, and the
persisted history jumped from the earlier Slack draft directly to "are you
stuck?". Compaction, provider translation, session reload, and a stale binary
were ruled out.

## Decision

Normal completed turns remain transactional. Cancellable frontends split an
agent turn into a staged attempt followed by an explicit commit or cancelled
finalization.

`TurnSignal` remains the single outcome claim. A staged result may enter
canonical history only after completion wins the existing
`Live -> Completed` compare-and-swap. If cancellation wins first, the staged
result cannot commit; the cancellation path materializes the turn journal
instead. Calling staged commit without first winning completion is a protocol
violation guarded beside the commit API and covered by forced-race tests.

Each active attempt owns a bounded, append-only turn-suffix journal. It records:

- whether finalization appends a new turn or replaces an explicit retry;
- the exact external user message;
- complete provider-authored messages, including opaque `ProviderState`;
- bounded `ToolResult` blocks immediately after they become authoritative;
- the identities of calls that have no authoritative result yet;
- usage from completed provider calls and the last measured prompt occupancy.

Canonical pre-turn history is not copied into the journal. New turns append the
materialized suffix. Explicit retries replace history from the recorded user
turn boundary while preserving that turn's client message id.

Cancellation does not wait for an active tool. A result is authoritative only
after output bounding and journal insertion. Every call without such a result
gets one adjacent error `ToolResult` whose model-facing text says that
cancellation left its outcome and side effects uncertain and that current
state must be inspected before retrying. A completed `bg` call keeps its exact
result and process handle; an unjournaled spawn is uncertain like any other
call. This rule is conservative because foreground children are killed on
drop, while blocking scripts, remote servers, and already-launched background
work may outlive the dropped future.

Unfinalized stream deltas are not authoritative provider messages. They remain
visible in the live client but are not inserted into provider-facing or
persisted history without a final response and its provider-owned continuation
state. The persisted cancellation note explicitly says that any partial
response was discarded. Consequently, a reloaded thread may omit partial text
that was visible immediately before cancellation; this asymmetry is deliberate
and tested.

Cancellation uses the existing persisted `AssistantOutcome::Aborted` slot.
Canonical `TurnStatus::Cancelled` and observability reason
`client_cancelled` retain the precise runtime cause. Not adding a new serialized
outcome keeps older Rust and Android readers compatible with sessions written
by the new binary.

The cancellation note and uncertain tool-result text are one embedded,
runtime-overridable prompt resource. Frontends never reconstruct provider
history independently. SessionCore finalizes history, usage, client ids,
outcome, and persistence before releasing the active-turn permit; ACP only
projects events, and chat Ctrl-C invokes the same core AgentSession finalizer.

## Rejected alternatives

- **Rollback the whole turn:** memory would remain internally neat while
  forgetting real side effects, which caused the incident.
- **Mutate canonical history throughout the turn:** retries and reactive
  compaction need rollback savepoints, and an active tool batch temporarily has
  provider-invalid call/result pairing.
- **Reconstruct from frontend events:** events omit exact multimodal messages,
  tool inputs, bounded model-visible output, and opaque provider state.
- **Persist partial stream text as plain assistant text:** this fabricates an
  authoritative response without provider continuation state and can make
  later requests invalid.
- **Wait for active tools during cancellation:** remote or detached work has no
  reliable bounded completion contract, and waiting would make cancellation
  ineffective.
- **Add `AssistantOutcome::Cancelled`:** older persisted-session and Android
  decoders reject unknown enum variants.

## Consequences

- The next prompt sees the cancelled user request exactly once, all
  authoritative completed results, and explicit uncertainty for every other
  call.
- A live client may retain more partial display text than a later reload, but
  the model never mistakes that text for an authoritative response.
- Completed usage is retained on cancellation without double-counting normal
  commits.
- Provider adapters must accept materialized multi-call histories with opaque
  provider state, completed results, and synthetic unresolved results; each
  adapter has regression coverage.
- The staged AgentSession API is wider than the previous prompt helper and must
  keep its completion-claim precondition adjacent to the commit operation.
