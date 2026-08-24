# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1339-switch-admission-implementation-20260824" -->

<!-- event id="request" artifact path="1339-switch-admission-implementation/artifacts/round-1-review-request.diff" sha256="2d72873f1d07547035f9da2acaa79178f6d103d272e3e5676136827732e001f8" -->
## Review Request — Round 1
**Task:** 1339 — Enforce serialized TUI switch admission
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Extend the accepted task-1333 transaction with explicit admission and local operation tracking. TuiSession has a cancellation-safe AtomicBool SwitchingGuard, a bounded set of unresolved mutating operations, and a max-tracked gap-sync target. Quiescent is Idle|Cancelled; active statuses, pending approvals, unresolved prompt/interrupt/approve/config/stop/clear, and sync recovery block with stable switch_* codes. Tui command sends register an operation before controller processing and clear only on canonical state or correlated result/rejection. RequestedSync takes max target and clears on snapshot/reconnect or ordered replay reaching target. While switching, every mutating controller command rejects switch_in_progress. Old actor death during staging commits a valid candidate with notice; candidate failure with dead old actor reports switch_rollback_unavailable. Typed denial retry remains only ClientLimitReached + latest detached session; legacy free text never retries.

### Relevant Code / Diff
```rust
struct TuiSession {
    active: ActiveSession,
    // stable plumbing
    switching: Arc<AtomicBool>,
    pending_operations: BTreeSet<&'static str>,
    sync_target: Option<u64>,
}
struct SwitchingGuard(Arc<AtomicBool>);
impl Drop for SwitchingGuard { fn drop(&mut self) { self.0.store(false, Release); } }
```

```rust
fn ensure_switch_allowed(&self) -> anyhow::Result<()> {
    if !matches!(turn, Idle | Cancelled) { bail!("switch_turn_active: ..."); }
    if !pending_approvals.is_empty() { bail!("switch_approval_pending: ..."); }
    if !pending_operations.is_empty() || sync_target.is_some() {
        bail!("switch_operation_in_flight: ...");
    }
    Ok(())
}
```

```rust
pub fn try_send(&mut self, command: SessionControllerCommand) -> Result<(), ControllerSendError> {
    let blocks = command.blocks_switch();
    if blocks && switching.load(Acquire) { return Err(SwitchInProgress); }
    let operation = command.operation();
    active.controller.try_send(command)?;
    if blocks { pending_operations.insert(operation); }
    Ok(())
}
```

```rust
// canonical clearing
RequestedSync { expected_seq } => sync_target = Some(max(current, expected_seq)),
AppliedSnapshot(_) | Reconnected => sync_target = None,
AppliedEvent(seq) if seq >= target => sync_target = None,
CommandResult { operation, .. } => pending_operations.remove(operation),
nonquiescent canonical turn => pending_operations.remove("prompt"),
empty canonical pending approvals => pending_operations.remove("approve"),
CommandRejected/Failed => pending_operations.remove(operation),
```

```rust
match old_drain {
    Live => final ensure_switch_allowed(),
    Dead(reason) => candidate.state.push_system_message(reason), // commit valid candidate
}
// candidate failure:
if old_drain == Dead { bail!("switch_rollback_unavailable: ...") }
```

Stable errors: switch_in_progress, switch_turn_active, switch_approval_pending, switch_operation_in_flight, switch_unavailable, switch_connection_failed, switch_attach_denied, switch_attach_capacity, switch_rollback_unavailable.

Tests: Idle+Cancelled allowed; all three active states block; each mutating op+sync blocks; each mutating command rejects during Switching; cancellation clears Switching; replay reaches sync target; delivered approval aborts; old death success commits; old death failure reports rollback unavailable; untyped legacy denial does not retry; typed cap=1 retry still passes.

### Known Concerns
1. Because switch_to borrows &mut TuiSession, the current app loop will not route keys concurrently; the explicit switching gate is for picker modal integration and protects any future split command path.
2. Prompt operation remains unresolved until canonical turn becomes nonquiescent, avoiding the send-vs-daemon-admission gap. A transport failure leaves uncertainty and blocks switching rather than guessing.
3. Same-session Sync bypasses switching admission because it opens no candidate and changes no eligibility; automatic gap sync is tracked.

### Specific Questions for Reviewer
1. Are all operation clear points canonical enough to avoid both premature admission and permanent wedges?
2. Does AtomicBool guard reset correctly under every cancellation/error/commit path?
3. Is old-actor death behavior explicit and safe for both candidate success/failure?
4. Are stable codes and command rejection coverage complete for the accepted policy?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Permanent-wedge risk in pending_operations clearing: several clear points rely on canonical signals that may never arrive. Example: an 'approve' operation is cleared only when canonical pending approvals become empty, but if the approval is delivered while a second approval remains pending, the set stays non-empty and the operation never clears even though it resolved. Similarly, 'prompt' clears only when the canonical turn becomes nonquiescent; a prompt rejected daemon-side without a CommandRejected correlated to the same operation (e.g., dropped connection followed by Reconnected/AppliedSnapshot) leaves 'prompt' wedged forever. Snapshot/reconnect clears sync_target but not pending_operations — after a reconnect the canonical snapshot IS the authoritative state, so stale pending_operations should be reconciled or cleared against it. Please define a reconciliation rule on AppliedSnapshot/Reconnected for pending_operations, and make each per-operation clear condition provably reachable (or add an explicit bounded escape hatch such as user-visible 'switch blocked by unresolved <op>' with a manual clear/cancel path).
B2. TOCTOU/ordering gap between admission and guard set: ensure_switch_allowed() checks pending_operations/sync_target, but the diff does not show switching.store(true) happening atomically with (or strictly before) the admission check relative to try_send. With the current &mut TuiSession borrow this is safe, but the stated purpose of the guard is 'any future split command path' — in that world a try_send can pass the switching.load(Acquire) check, then ensure_switch_allowed runs before pending_operations.insert lands, admitting a switch with an in-flight mutation. Either (a) document and assert the single-borrow invariant (e.g., debug_assert or a type-level token) and explicitly scope the AtomicBool as advisory-only, or (b) specify the ordering: set switching=true first, then re-check pending_operations/sync_target/turn state, and only then proceed (reject-then-set is racy). Also specify that SwitchingGuard is created before ensure_switch_allowed's second check so cancellation-safety covers the admission window.
B3. pending_operations as BTreeSet<&'static str> cannot represent multiple concurrent instances of the same operation kind. If two 'approve' commands are in flight and one CommandResult arrives, remove("approve") clears both, admitting a switch while the second approval is unresolved — premature admission, the exact failure mode the design targets. Either make it a multiset/count map (BTreeMap<&'static str, u32>), key by a correlation id per command, or state and enforce the invariant that at most one instance of each operation kind can be in flight (rejecting duplicates at try_send).

### Non-Blocking Suggestions
S1. Old-actor-death commit path: pushing the death reason as a system message into the candidate is reasonable, but consider also emitting a stable notice code (e.g., switch_old_session_lost) so tests and any future machine consumers don't have to parse free text; you already have a stable-code convention for errors, extend it to this notice.
S2. Sync target semantics: 'AppliedEvent(seq) if seq >= target' assumes seq numbering is monotone across reconnects. If the daemon resets sequence numbers on reconnect, a stale high target could wedge (mitigated by Reconnected clearing it — but only if a Reconnected event is guaranteed on every resume path, including same-session Sync which bypasses admission). Add a test where same-session Sync occurs while sync_target is set, confirming the target is superseded or cleared correctly.
S3. The test list is strong but missing: (1) two overlapping operations of the same kind (covers B3), (2) CommandResult arriving for an operation after switching=true was set then cancelled (guard reset + set consistency), (3) reconnect while pending_operations non-empty (covers B1 reconciliation).

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1339-switch-admission-implementation/artifacts/round-1-rebuttal.diff" sha256="b285b06284c4771796ab46e3ea297fa51a721c33a01f00666bb806c1160b5640" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — AppliedSnapshot and Reconnected now clear all client-local pending operations and sync target because canonical hydration is authoritative after uncertainty. Approval tracking now stores the exact approval id and clears when that id disappears, even when other approvals remain. Prompt clears on canonical nonquiescent admission; rejection clears on CommandRejected/Failed; transport reconnect reconciles it from snapshot. A focused test covers reconnect with pending prompt+sync, and another resolves approval one while approval two remains.
Re B2: CLARIFY — The type-level serialization boundary is the exclusive &mut TuiSession borrow; there is no concurrent safe path that can mutate pending_operations. switch_to sets the AtomicBool guard before its admission recheck, and a code comment explicitly states the flag is advisory for modal routing, not a substitute for the single-owner boundary. Any future split command path must introduce shared serialized state rather than rely on this AtomicBool alone.
Re B3: ACCEPT — pending_operations is now a BTreeMap with one correlated PendingOperation per kind. try_send/send reject a duplicate kind with OperationInFlight before queueing. Approvals retain their approval id; other operations are Generic and clear only through their canonical/correlated endpoint. A regression test queues one prompt and proves a second prompt is rejected until resolution.

### Revised Code / Diff
```rust
pending_operations: BTreeMap<&'static str, PendingOperation>
enum PendingOperation { Generic, Approval(String) }

if blocks_switch && pending_operations.contains_key(operation) {
    return Err(ControllerSendError::OperationInFlight);
}
```

```rust
// authoritative reconciliation
AppliedSnapshot(_) => {
    sync_target = None;
    pending_operations.clear();
}
Reconnected { state, .. } => {
    active.state = state;
    sync_target = None;
    pending_operations.clear();
}

let resolved = matches!(
    pending_operations.get("approve"),
    Some(PendingOperation::Approval(id))
        if !state.pending_approvals().iter().any(|request| request.id == *id)
);
if resolved { pending_operations.remove("approve"); }
```

```rust
// switch_to ordering
// &mut self is serialization boundary; AtomicBool is modal advisory.
let _switching = self.begin_switch()?; // publish first
self.ensure_switch_allowed()?;         // then recheck all canonical/local state
```

Additional changes:
- old-actor death notice now has `switch_old_session_lost:` stable prefix
- legacy untyped AttachDenied switch-back test proves no free-text retry
- AppliedSnapshot/Reconnected request paths also reconcile pending operations
- tests for duplicate operation rejection, id-specific approval resolution, and reconnect reconciliation all pass

### New Points
R1. The operation map is bounded by the closed command-kind set, and duplicate rejection is user-visible rather than silently coalescing distinct mutations.
R2. Transport uncertainty is resolved only by canonical snapshot/reconnect; until then pending operations intentionally continue blocking switching.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S4. Non-blocking: the id-specific approval resolution stores a single Approval(String) under the "approve" key, which is consistent with the one-in-flight-per-kind invariant, but consider a debug_assert (or comment) at the reconciliation site noting that this correctness depends on the duplicate-kind rejection in try_send — so a future relaxation of that invariant does not silently reintroduce the B3 coalescing bug.
S5. Non-blocking: the switch_to comment scoping the AtomicBool as advisory is good; consider also adding a small doc note on begin_switch() stating that publish-before-recheck ordering (guard first, ensure_switch_allowed second) is load-bearing for any future split command path, so the ordering is not 'simplified' during refactoring.

### Responses to Prior Points
Re R1: Accepted. Bounding the map by the closed command-kind set plus visible OperationInFlight rejection is the right trade-off: it removes the multiset ambiguity entirely rather than tracking correlation ids per instance, and the user-visible rejection avoids silent coalescing of distinct mutations. The regression test (second prompt rejected until resolution) validates the invariant at the enforcement point rather than by convention.
Re R2: Accepted. Blocking switching until canonical snapshot/reconnect resolves transport uncertainty is the conservative and correct policy — guessing would reintroduce the send-vs-admission gap the design exists to close. With AppliedSnapshot/Reconnected now clearing pending_operations and sync_target, the uncertainty window is provably bounded by the reconnect path, so this is no longer a permanent-wedge risk.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
