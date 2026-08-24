# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1331-clear-usage-20260824" -->

<!-- event id="request" artifact path="1331-clear-usage/artifacts/round-1-review-request.diff" sha256="b1c4ec855672ef3f97b5680ce11c410b3221dec6f7a6f154155804553227c0d4" -->
## Review Request — Round 1
**Task:** 1331 — Daemon-authoritative TUI clear and cumulative usage commands
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Add additive protocol-v2 ClearHistory and GetUsage commands. Clear is Configure-gated, serialized through daemon admission/lifecycle, rejected during active turns, clears AgentSession history plus client request IDs and assistant outcomes, persists the empty authoritative record, emits sequenced ConversationCleared followed by zero context usage, and returns a correlated CommandResult. GetUsage is Observe-gated, admission-serialized, rejected during active turns, and returns a typed SessionUsage with integer token fields and bounded microdollar cost. SessionClient/Controller route both commands; TUI maps /clear and /usage while idle, folds canonical clear in both live reducer and daemon snapshot accumulator, and renders usage as a frontend-local notice. Android v2 fixtures are extended additively.

### Relevant Code / Diff
```rust
// src/session_protocol.rs
pub enum ClientMessage {
    // existing variants...
    ClearHistory { request_id: String },
    GetUsage { request_id: String },
}

pub enum ServerMessage {
    // existing variants...
    Usage { request_id: String, usage: SessionUsage },
}

pub struct SessionUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning_output: Option<u64>,
    pub thinking_bytes: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd_micros: u64,
}

pub enum SessionEvent {
    // existing variants...
    ConversationCleared,
}
```

```rust
// src/session_core.rs
pub async fn clear_history(&self) -> Result<bool, HistoryMutationError> {
    if self.turn.is_active() { return Err(HistoryMutationError::Busy); }
    let _lifecycle = self.lifecycle.lock().await;
    if self.turn.is_active() { return Err(HistoryMutationError::Busy); }

    let mut session = self.session.lock().await;
    let mut client_ids = self.client_user_message_ids.lock().await;
    let history_changed = !session.history().is_empty() || !client_ids.is_empty();
    session.clear();
    client_ids.clear();
    let model = session.model().to_string();
    drop(client_ids);
    drop(session);
    let outcomes_changed = {
        let mut outcomes = self.assistant_outcomes.lock().unwrap_or_else(|p| p.into_inner());
        let changed = !outcomes.is_empty();
        outcomes.clear();
        changed
    };
    let changed = history_changed || outcomes_changed;
    let context_window = self.context_windows.lock().await.get(&model).copied();
    self.persist(&model, &[], &[]);
    let _ = self.events.emit(SessionEvent::ConversationCleared);
    self.publish_context_usage(self.context_usage(0, context_window));
    Ok(changed)
}

pub async fn cumulative_usage(&self) -> SessionUsage {
    let usage = self.session.lock().await.total_usage().clone();
    SessionUsage {
        input: usage.input,
        output: usage.output,
        reasoning_output: usage.reasoning_output,
        thinking_bytes: usage.thinking_bytes,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        cost_usd_micros: usd_to_micros(usage.cost.total_usd),
    }
}

fn usd_to_micros(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 { return 0; }
    (value * 1_000_000.0).round().min(u64::MAX as f64) as u64
}
```

```rust
// src/session_daemon.rs handler excerpts
Some(ClientMessage::ClearHistory { request_id }) => {
    validate; require Configure;
    let _admission = attachment.entry.admission.lock().await;
    match attachment.core().clear_history().await {
        Ok(changed) => send(CommandResult { request_id, operation: "clear_history", changed }),
        Err(Busy) => send(Error { request_id: Some(request_id), code: "session_busy", ... }),
    }
}
Some(ClientMessage::GetUsage { request_id }) => {
    validate; require Observe;
    let _admission = attachment.entry.admission.lock().await;
    if core.turn_is_active() { send session_busy; continue; }
    send(ServerMessage::Usage { request_id, usage: core.cumulative_usage().await });
}
```

```rust
// src/frontend_state.rs and daemon SnapshotState
SessionEvent::ConversationCleared => {
    transcript.clear();
    tool_calls.clear();
    pending_approvals.clear();
    history_truncated = false;
    ending_reason = None; // frontend reducer
    next_transcript_id = 1; // daemon snapshot accumulator
}
```

```rust
// src/tui/app.rs
UiCommand::Clear if quiescent => queue(SessionControllerCommand::ClearHistory),
UiCommand::Usage if quiescent => queue(SessionControllerCommand::GetUsage),
UiCommand::Clear | UiCommand::Usage => notice("command unavailable while a turn is running"),
```

```rust
// src/tui/session.rs
SessionControllerEvent::StateChanged { mut state, outcome } => {
    if let SessionClientOutcome::Usage { usage, .. } = outcome {
        state.push_system_message(format!(
            "input={} output={} cache_read={} cache_write={} cost=${:.4}",
            usage.input, usage.output, usage.cache_read, usage.cache_write,
            usage.cost_usd_micros as f64 / 1_000_000.0,
        ));
    }
    self.state = state;
}
```

SessionClient adds clear_history/get_usage methods with capability checks and generated request IDs; SessionController adds matching commands and outcomes. Protocol validation caps both request IDs. Android v2 fixtures add both client commands and ConversationCleared event.

Tests added:
- exact additive wire shapes for clear/get_usage/Usage
- canonical reducer clear sequence
- core clear resets memory, persistence, snapshot, replay event
- daemon handler returns typed zero usage and clear event/context/result in any scheduling order
- usage response renders local TUI notice
- Android v2 fixtures fold clear and deserialize new commands
- microdollar conversion finite/negative/NaN/infinity behavior

Focused tests pass; cargo check passes.

### Known Concerns
1. SessionStore persistence remains best-effort and cannot report a clear-save failure until task 1335 adds durability status.
2. Cumulative usage intentionally remains live-process state, matching existing chat/TUI semantics; current persisted session format never stored usage.
3. Usage is a direct response/local notice rather than canonical event, so querying one client does not pollute replay for all clients.

### Specific Questions for Reviewer
1. Is clear correctly atomic relative to prompt start, runtime changes, persistence, and canonical event ordering?
2. Should cumulative usage be snapshot/persisted now, or is live direct-response parity correct for this task?
3. Does resetting transcript IDs to 1 after a sequenced clear create any ambiguity for clients?
4. Are Configure for clear and Observe for usage the correct existing capability gates?

<!-- event id="review_response_retry1" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. TOCTOU between busy-check and clear: clear_history checks turn.is_active(), takes the lifecycle lock, and re-checks, but the diff does not show that prompt/turn start also acquires the same lifecycle lock before flipping turn.is_active(). If turn activation is not serialized under that lock, a turn can begin between the second check and the history mutation, clearing history out from under an in-flight turn. Confirm (with a test) that turn start acquires the lifecycle lock, or move the busy check and mutation under a single mutual-exclusion point shared with turn admission.
B2. Transcript ID reset to 1 in the daemon SnapshotState creates ID-reuse ambiguity. A client that attached before the clear may hold references to transcript IDs (e.g., pending render state, deltas keyed by ID) that will be reissued for new, unrelated entries after ConversationCleared. Since ConversationCleared is sequenced, any client that misses or reorders processing relative to buffered per-ID updates can conflate old and new entries. Either keep the ID counter monotonic across clears, or document and test that all per-ID server messages are strictly ordered after ConversationCleared on every attachment stream so reuse is provably unambiguous.
B3. Clear ordering can leak stale state on failure paths and to concurrently attaching clients: clear_history persists the empty record, emits ConversationCleared, then publishes zero context usage, but the diff shows no serialization between this sequence and snapshot generation for a newly attaching client. A client attaching between session.clear() and the event emission could receive a pre-clear snapshot followed by no ConversationCleared (or a post-clear snapshot followed by a redundant one). State whether attachment/snapshot generation is serialized under the same admission/lifecycle locking, and add a test covering attach-during-clear.
B4. Cross-lock atomicity in clear_history: session and client_user_message_ids locks are dropped before assistant_outcomes is cleared, and persist() is called after all locks are released. Nothing shown prevents another mutation (e.g., a late assistant outcome delivery or persistence of a concurrent write) interleaving between session.clear() and persist(&model, &[], &[]), which could persist or retain non-empty state after a 'successful' clear. Either hold a single mutation guard across the whole clear+persist sequence or demonstrate that all writers are excluded by the lifecycle lock held for the full duration.

### Non-Blocking Suggestions
S1. Answering question 4: Configure for ClearHistory (mutating) and Observe for GetUsage (read-only) match the existing gate semantics and look correct; no change requested.
S2. Answering question 2: live direct-response usage parity is acceptable for this task; persisting cumulative usage should be deferred (reasonably to the 1335 durability work). Consider adding a doc comment on SessionUsage noting it is process-lifetime scoped so clients don't assume durability.
S3. usd_to_micros silently maps NaN/negative/infinite costs to 0 or u64::MAX-saturation; consider logging (debug-level) when a non-finite or negative cost is encountered so upstream cost-accounting bugs aren't masked.
S4. GetUsage rejecting during active turns is stricter than necessary for a read-only query; if the busy rejection exists only to avoid torn reads, a brief lock on the session suffices and would improve UX (usage visible mid-turn). Fine to keep as-is for consistency, but note the rationale in the handler.
S5. TUI usage notice formats cost with {:.4} from microdollars; sub-$0.0001 costs render as $0.0000. Consider rendering micros directly or using more precision when the value is small but nonzero.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1331-clear-usage/artifacts/round-1-rebuttal.diff" sha256="8cff8954d3dc5c1c63cb9cee86bc3bd24107f11b661e24b16bfd6edba7217c4d" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — The daemon admission gate already serialized prompt begin and clear, but SessionCore::clear_history itself did not own an atomic exclusion primitive. I added a SessionMutationPermit backed by AtomicBool. clear acquires it before checking the turn; begin_turn checks before and immediately after atomically claiming the turn slot, dropping that claim if a mutation won. This closes every interleaving even outside the daemon adapter. A deterministic test pauses clear on the lifecycle lock, waits for the mutation permit, and proves begin_turn returns Busy.
Re B2: ACCEPT — Resetting next_transcript_id was unnecessary and could reuse live projection identity. ConversationCleared still empties transcript/tool/approval state, but the snapshot accumulator now preserves its monotonic next ID. The daemon command test emits a post-clear user event and asserts its transcript ID is greater than the pre-clear ID.
Re B3: CLARIFY — Attach and clear already share SessionEntry.admission: attach_internal acquires an OwnedMutexGuard before registering the client and holds it through snapshot fitting/sending until finish_handshake; the clear handler holds the same admission guard through clear persistence and synchronous canonical event emission. SnapshotState subscribes synchronously to SessionEventRouter, so ConversationCleared and zero ContextUsage are folded before clear releases admission. I added a test that holds this gate and proves attach cannot complete until release.
Re B4: ACCEPT — The late persistence race was real: prompt_with_active_turn dropped ActiveTurn before its terminal persistence write, so clear could observe idle and persist empty state immediately before the prompt rewrote old history. ActiveTurn now remains held through terminal persistence and is dropped afterward. Combined with SessionMutationPermit blocking new begin_turn calls and the lifecycle lock excluding runtime mutation, no outcome/history/persistence writer can overlap a successful clear.

### Revised Code / Diff
```diff
--- a/src/session_core.rs
+++ b/src/session_core.rs
@@
+struct SessionMutationPermit<'a>(&'a AtomicBool);
+impl SessionMutationPermit<'_> {
+    fn acquire(active: &AtomicBool) -> Result<Self, HistoryMutationError> {
+        active.compare_exchange(false, true, AcqRel, Acquire)
+            .map(|_| Self(active)).map_err(|_| HistoryMutationError::Busy)
+    }
+}
+impl Drop for SessionMutationPermit<'_> {
+    fn drop(&mut self) { self.0.store(false, Release); }
+}
@@
 pub struct SessionCore {
+    mutation_active: AtomicBool,
 }
@@
 pub fn begin_turn(&self) -> Result<SessionTurn<'_>, TurnError> {
+    if self.mutation_active.load(Acquire) { return Err(TurnError::Busy); }
     let active = self.turn.begin()?;
+    if self.mutation_active.load(Acquire) {
+        drop(active);
+        return Err(TurnError::Busy);
+    }
     // emit Running and return
 }
@@
 pub async fn clear_history(&self) -> Result<bool, HistoryMutationError> {
+    let _mutation = SessionMutationPermit::acquire(&self.mutation_active)?;
     if self.turn.is_active() { return Err(Busy); }
     let _lifecycle = self.lifecycle.lock().await;
     // clear memory, persist empty state, emit clear + zero context
 }
@@ prompt terminal ordering
-drop(active_turn);
 if let (Some(messages), Some(ids)) = (...) {
     self.persist(&model, &messages, &ids);
 }
+// Keep active permit through terminal persistence so /clear cannot be
+// followed by a late old-history rewrite.
+drop(active_turn);
```

```diff
--- a/src/session_daemon.rs
+++ b/src/session_daemon.rs
@@ SnapshotState::apply
 SessionEvent::ConversationCleared => {
     transcript.clear();
     tool_calls.clear();
     pending_approvals.clear();
     history_truncated = false;
-    next_transcript_id = 1;
 }
```

Tests added/revised:
- canonical_clear_resets_memory_persistence_and_snapshot pauses clear after mutation permit acquisition and asserts begin_turn is Busy.
- attach_waits_for_the_same_admission_gate_as_clear holds SessionEntry.admission and proves attach blocks until release.
- clear_and_usage_commands_are_daemon_authoritative now emits a post-clear UserMessage and asserts its transcript ID exceeds the pre-clear ID.

Additional non-blocking follow-ups applied:
- SessionUsage documents process-lifetime scope.
- invalid/non-finite/negative cumulative costs emit debug diagnostics before mapping to zero.
- GetUsage handler documents why active turns are rejected instead of waiting on the whole-turn AgentSession mutex.

### New Points
R1. SessionEntry.admission is held by attach through snapshot send and by clear through synchronous event folding; therefore a newly attaching client observes either the complete pre-clear state or complete post-clear state, never the mutation gap.
R2. Turn activity now spans terminal persistence, making the turn slot—not individual field locks—the atomic boundary for all history/outcome/persistence writes relative to clear.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. The begin_turn double-check (load permit, claim turn slot, re-load permit, drop claim on conflict) is correct but subtle; consider a short module-level comment documenting the protocol (mutation permit vs. turn slot ordering, and why AcqRel/Acquire suffice) so future edits don't reorder the checks and silently reintroduce the TOCTOU.
S7. Since the snapshot accumulator's next_transcript_id is now monotonic across clears, consider asserting that invariant in the canonical reducer property/replay tests too (not only the daemon command test), so a future 'fresh session' refactor of the frontend reducer doesn't diverge from daemon behavior.

### Responses to Prior Points
Re R1: Accepted. The attach path holding SessionEntry.admission from client registration through snapshot fitting/send, combined with clear holding the same admission guard through persistence and synchronous canonical event folding, gives the atomicity I asked for in B3: an attaching client observes either a fully pre-clear snapshot (and will later receive the sequenced ConversationCleared) or a fully post-clear snapshot with no gap state. The new attach-blocks-until-release test makes this invariant regression-protected rather than incidental.
Re R2: Accepted. Extending ActiveTurn to span terminal persistence is the right fix and is stronger than what B4 asked for: it makes the turn slot the single atomic boundary for all history/outcome/persistence writes, so the mutation permit + turn-slot check in begin_turn (with the re-check after claiming) closes the B1 TOCTOU and the B4 late-persist race with the same primitive. The deterministic pause-during-clear test asserting begin_turn returns Busy covers the interleaving I was concerned about.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
