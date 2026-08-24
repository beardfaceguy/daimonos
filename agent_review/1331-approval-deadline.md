# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1331-approval-deadline-20260824" -->

<!-- event id="request" artifact path="1331-approval-deadline/artifacts/round-1-review-request.diff" sha256="28a174623a4f67d6695fa68e1442b66e9a57e962ea5ee975ec829d0a0df0e7d1" -->
## Review Request — Round 1
**Task:** 1331 — Advertise anchored approval timeout state without changing broker semantics
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Extend ApprovalRequest additively with optional ineligible_deadline_unix_ms and deadline_paused. When the broker first observes no eligible approval client, anchor one Tokio deadline and corresponding wall-clock display deadline, update the broker's pending request, and emit sequenced ApprovalDeadlineChanged. Eligibility restoration marks that same deadline paused; later loss resumes it without extension. Broker pending snapshots therefore carry current state, and live/replay reducers update in place. TUI modal renders paused or remaining auto-deny state. Android v2 models/reducer/fixtures gain the additive fields/event. Existing resolution and timeout semantics remain unchanged.

### Relevant Code / Diff
```rust
// src/session_protocol.rs
pub struct ApprovalRequest {
    // existing fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ineligible_deadline_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deadline_paused: bool,
}
pub enum SessionEvent {
    ApprovalDeadlineChanged {
        approval_id: String,
        ineligible_deadline_unix_ms: u64,
        paused: bool,
    },
}
```

```rust
// ApprovalBroker
fn update_deadline(&self, id: &str, deadline_unix_ms: u64, paused: bool) -> bool {
    let pending = state.pending.get_mut(id)?;
    if pending.request.ineligible_deadline_unix_ms == Some(deadline_unix_ms)
        && pending.request.deadline_paused == paused { return false; }
    pending.request.ineligible_deadline_unix_ms = Some(deadline_unix_ms);
    pending.request.deadline_paused = paused;
    true
}
```

```rust
// request_approval timeout loop
let mut ineligible_deadline: Option<(tokio::time::Instant, u64)> = None;
loop {
    if let Ok(resolution) = receiver.try_recv() { break resolution; }
    let eligibility_changed = broker.eligibility_changed.notified();
    if broker.has_eligible_client(&approval_id) {
        if let Some((_, unix_ms)) = ineligible_deadline {
            if broker.update_deadline(&approval_id, unix_ms, true) {
                events.emit(ApprovalDeadlineChanged { id, unix_ms, paused: true });
            }
        }
        select! { resolution = &mut receiver => break ..., _ = eligibility_changed => continue }
    } else {
        let (deadline, unix_ms) = *ineligible_deadline.get_or_insert_with(|| (
            Instant::now() + timeout,
            system_deadline_unix_ms(timeout),
        ));
        if broker.update_deadline(&approval_id, unix_ms, false) {
            events.emit(ApprovalDeadlineChanged { id, unix_ms, paused: false });
        }
        select! {
            resolution = &mut receiver => break ...,
            _ = eligibility_changed => continue,
            _ = sleep_until(deadline) => resolve(Deny, "approval_timeout"),
        }
    }
}
```

Frontend ViewState and daemon SnapshotState locate the pending approval by id and update deadline/paused fields. ApprovalRequested remains unchanged when initially eligible; broker.pending() supplies updated fields to fresh snapshots. TUI modal adds one status line: paused with original deadline retained, or auto-deny remaining seconds. Android ApprovalRequest, SessionEvent, reducer, fixture, and tests mirror the additive wire shape.

Tests:
- deadline becomes advertised on first ineligibility, pauses on eligible reconnect, resumes with exactly the original deadline
- existing churn test still proves reconnect cannot extend expiry
- frontend reducer updates pending request fields
- TUI renders paused anchored status
- Android/Rust v2 fixtures decode and fold ApprovalDeadlineChanged
- all focused approval tests pass

### Known Concerns
1. Wall-clock unix_ms is presentation only; timeout enforcement remains monotonic Tokio Instant.
2. A reattaching client may receive the pre-pause snapshot followed immediately by the sequenced paused update; admission/event ordering still converges canonically.
3. Deadline event emission is best-effort like other canonical events; sequence exhaustion remains the existing fail-closed boundary.

### Specific Questions for Reviewer
1. Does the update loop preserve the exact first-ineligible anchored deadline under all eligibility churn?
2. Can eligibility notifications be lost between state check and Notify future creation?
3. Should deadline state be embedded only in ApprovalRequest, only event-driven, or is the combined snapshot+event model correct?
4. Does exposing wall-clock deadline create any misleading behavior under system clock changes?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Lost-wakeup race in the eligibility wait: in the eligible branch, `broker.eligibility_changed.notified()` is created before the `has_eligible_client` check in the sketch's ordering, but in the ineligible branch the code between creating the Notify future and awaiting it performs `update_deadline` and event emission; more importantly, in both branches any eligibility change occurring between the state check and the `select!` awaiting `eligibility_changed` is only caught if `notified()` was created *before* the check AND `Notify::notified` buffers a permit. Tokio's Notify only stores a single permit for `notify_one`, and `notify_waiters` does NOT store a permit for futures not yet awaited (only for futures already created/enabled). Please state explicitly whether the broker uses `notify_one`/`notify_waiters` and prove the future-creation-before-check ordering holds on every loop iteration, otherwise the loop can sleep past an eligibility change (e.g., staying paused/unpaused incorrectly, or failing to arm the deadline while ineligible).
B2. Busy-loop / missed-resolution hazard: the loop head uses `receiver.try_recv()` then later `select!` on `&mut receiver`. If `receiver` is a oneshot, polling it via `try_recv` and then re-awaiting `&mut receiver` across iterations is fine, but the `continue` on eligibility change re-enters the loop and calls `try_recv` again; if the resolution arrives concurrently with an eligibility notification, ensure resolution always wins deterministically and that no path can drop the resolution (e.g., oneshot consumed by try_recv error state). Specify the receiver type and its cancel-safety across select re-polls.
B3. Paused-while-expired edge case is unspecified: if eligibility is restored a few milliseconds before the anchored Tokio deadline fires, the loop marks the deadline paused and stops arming `sleep_until`. If the client then never acts, the approval now lives forever despite having been within its ineligible-timeout window for effectively the entire duration. Confirm this indefinite-pending-while-eligible behavior is the intended existing semantics (i.e., timeout only ever applies while ineligible) and add a test covering restoration at T-epsilon; otherwise the claim 'existing timeout semantics remain unchanged' needs a precise statement of what those semantics are.
B4. Snapshot/event ordering guarantee is asserted but not shown: 'a reattaching client may receive the pre-pause snapshot followed immediately by the sequenced paused update' assumes the snapshot is taken atomically with a sequence cursor so no ApprovalDeadlineChanged is both baked into the snapshot AND replayed (harmless if idempotent) or, worse, dropped (snapshot taken before update_deadline mutates pending, but event sequenced before the client's replay cursor). Show that update_deadline mutation and event emission occur under the same lock/ordering domain as snapshot capture, or document that reducers treat ApprovalDeadlineChanged as idempotent last-write-wins keyed by (approval_id), including out-of-order-with-snapshot cases.

### Non-Blocking Suggestions
S1. Answering Q4: exposing wall-clock unix_ms is acceptable for display, but under system clock skew the TUI's 'remaining seconds' computed from wall clock can disagree with the actual monotonic expiry. Consider also emitting remaining_ms at event time (or documenting that clients should tolerate a deny arriving before/after the displayed deadline).
S2. Answering Q3: the combined snapshot+event model is the right choice given the existing architecture (fresh attaches need state without replay; live clients need deltas). Keep it, but document ApprovalDeadlineChanged as strictly idempotent so the redundancy is safe.
S3. `update_deadline` returns bool for dedup, but the anchored unix_ms never changes after first ineligibility; consider asserting (debug_assert) that a second call never carries a different unix_ms for the same approval_id, to catch anchor-drift bugs in future refactors.
S4. Wire compatibility: `deadline_paused: bool` with `skip_serializing_if = is_false` plus `serde(default)` is fine for Rust, but confirm the Android v2 decoder treats an absent field as false and that fixtures cover the absent-field case, not just present-false.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1331-approval-deadline/artifacts/round-1-rebuttal.diff" sha256="a4f47c665e6da997b030effdfd6a58e58538735a9a480f127e046b893302db19" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — ApprovalBroker uses Notify::notify_waiters, so merely constructing Notified was insufficient. Each loop now Box::pin()s Notified and calls enable() before reading eligibility; every transition after registration wakes that iteration. Both eligible and ineligible select branches await the already-enabled future. The code comment records why ordering cannot change.
Re B2: CLARIFY — The receiver is tokio::sync::oneshot::Receiver. try_recv consumes only Ok(value); Empty leaves the receiver valid, and polling &mut Receiver in select is cancel-safe. I made both select blocks biased with the resolution branch first, so simultaneous resolution/eligibility churn deterministically completes. A comment pins these semantics.
Re B3: ACCEPT — Indefinite pending while any eligible client is attached is the existing intended invariant: expiry is armed only in the ineligible branch, even if the anchored instant passes while paused. Added a T-epsilon-style regression: restore eligibility halfway to expiry, wait beyond the original deadline, prove approval remains pending, then resolve normally.
Re B4: CLARIFY — Broker state mutation precedes event sequencing. Thus snapshot races have only two outcomes: snapshot gets old state/old sequence then receives the update, or gets new state with old/new sequence and may receive the same idempotent update. Event-before-new-state is impossible. Live daemon SnapshotState subscription folds emit synchronously. Reducers already apply the update last-write-wins by approval_id; I documented that contract on ApprovalDeadlineChanged and fixtures exercise snapshot-with-absent defaults followed by the event. A debug_assert rejects anchor drift.

### Revised Code / Diff
```diff
--- a/src/session_core.rs
+++ b/src/session_core.rs
@@
 loop {
+    // oneshot try_recv consumes only ready values; Empty remains valid.
     match receiver.try_recv() { ... }
-    let eligibility_changed = broker.eligibility_changed.notified();
+    let mut eligibility_changed = Box::pin(broker.eligibility_changed.notified());
+    // notify_waiters retains no permit for unregistered futures.
+    eligibility_changed.as_mut().enable();
     if broker.has_eligible_client(...) {
         ...
         tokio::select! {
+            biased;
             resolution = &mut receiver => ...,
-            _ = eligibility_changed => continue,
+            _ = &mut eligibility_changed => continue,
         }
     } else {
         // same already-enabled future and biased resolution-first select
     }
 }
@@ ApprovalBroker::update_deadline
+debug_assert!(existing_deadline.is_none() || existing_deadline == Some(deadline_unix_ms));
```

```diff
--- a/src/session_protocol.rs
+++ b/src/session_protocol.rs
@@
-/// Wall-clock deadline...
+/// Display-only wall-clock deadline; enforcement is monotonic and clients
+/// tolerate wall-clock adjustments.
@@
 ApprovalDeadlineChanged {
+    /// Idempotent last-write-wins update; snapshots may already contain it.
 }
```

```diff
--- a/src/tui/render.rs
+++ b/src/tui/render.rs
@@
-format!("auto-deny in {remaining}s ...")
+format!("auto-deny in ~{remaining}s ...")
```

Tests added:
- eligibility_restored_before_expiry_pauses_even_past_deadline: eligible at half-time, wait past anchored instant, approval remains pending.
- approval_deadline_state_is_anchored_and_paused_not_reset: same unix deadline through pause/resume.
- existing churn timeout test remains green.
- Android/Rust fixtures decode ApprovalRequest without new fields (defaults) then apply ApprovalDeadlineChanged.

### New Points
R1. The authoritative order is broker mutation then event sequence allocation; therefore a snapshot can duplicate an idempotent update but cannot skip a state transition.
R2. Paused past deadline is not an edge exception—it is the core safety/UX invariant: an eligible attached operator is never auto-denied, while the original deadline remains ready to expire immediately if eligibility is lost again.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. The Box::pin + enable() before the eligibility check is correct for notify_waiters, but it is subtle enough to regress silently. Consider extracting it into a small helper (e.g., broker.enabled_eligibility_listener()) or adding a loom/targeted concurrency test that toggles eligibility between the check and the select to lock in the enable-before-read ordering beyond the code comment.
S6. With `biased; resolution first` in both selects, an eligibility change concurrent with resolution is deterministic, but note that biased selects poll the oneshot every wakeup; that is fine here, just document that the bias exists for determinism (resolution wins), not for fairness, so future edits don't remove it as an apparent micro-optimization.

### Responses to Prior Points
Re R1: Accepted. Mutation-before-sequencing establishes the required monotonic ordering: a snapshot may contain state whose corresponding event is subsequently replayed (harmless given documented idempotent last-write-wins semantics), but the dangerous case—an event sequenced before its state mutation, allowing a snapshot+cursor pair that misses the transition—is structurally excluded. Combined with the fixture covering snapshot-with-defaults followed by the event, this resolves B4.
Re R2: Accepted. Framing paused-past-deadline as the core invariant (eligible attached operator is never auto-denied; anchored deadline fires immediately upon renewed ineligibility) is a coherent and defensible semantics, and the eligibility_restored_before_expiry_pauses_even_past_deadline regression test pins it. One note for the immediate-expiry-on-renewed-ineligibility path: when eligibility is lost again after the anchored instant has passed, the ineligible branch re-arms sleep_until with an elapsed deadline, which fires immediately—confirm that path emits the resumed (paused=false) deadline event before or coincident with the deny so clients don't render a stale paused state after resolution; this is cosmetic given the terminal Deny event, so it does not block.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
