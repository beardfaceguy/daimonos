# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1333-atomic-switch-implementation-20260824" -->

<!-- event id="request" artifact path="1333-atomic-switch-implementation/artifacts/round-1-review-request.diff" sha256="304adbcd763238891861fd1f4da00e3d220597ea0264ad0113d7d02ee7315487" -->
## Review Request — Round 1
**Task:** 1333 — Atomic whole-controller TUI session switching
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Implement the accepted two-phase switch in TuiSession. Production supplies a ControllerFactory that opens a fresh Unix socket and spawns a fresh epoch-tagged SessionController with its own per-connection client ID and reconnect factory. switch_to keeps the active instance/view, attaches and snapshot-hydrates a candidate off-screen, drains already-delivered old outcomes, rechecks idle/cancelled plus no pending approvals, then commits via std::mem::swap of the entire TuiSession. The old whole instance shuts down asynchronously and detaches. Candidate failure/drop preserves current state. Same-session selection sends SyncRequest. Typed ClientLimitReached retries only when switching to the one session this TUI detached from most recently; attempts/backoff are configured. Actor AttachFailed preserves typed code. Tests cover success+detach, failure rollback, delivered approval race, cap=1 switch-back retry, and same-ID resync.

### Relevant Code / Diff
```rust
pub type ControllerFactory = Arc<dyn Fn() -> ControllerFuture + Send + Sync>;
pub struct SwitchPolicy { retry_attempts: usize, retry_backoff: Duration }

pub async fn switch_to(&mut self, target: &str) -> anyhow::Result<()> {
    if target == self.state.session_id() {
        self.send(SessionControllerCommand::Sync).await?;
        return Ok(());
    }
    self.ensure_switch_allowed()?;
    let transient_retry = self.last_detached_session.as_deref() == Some(target);
    let attempts = if transient_retry { policy.retry_attempts } else { 1 };
    for attempt in 0..attempts {
        if attempt > 0 { sleep(policy.retry_backoff).await; }
        let controller = factory().await?;
        match Self::attach_to(controller, Some(target.to_string()), ...).await {
            Ok(mut candidate) => {
                self.drain_before_switch_commit()?;
                self.ensure_switch_allowed()?;
                candidate.last_detached_session = Some(self.state.session_id().to_string());
                std::mem::swap(self, &mut candidate);
                tokio::spawn(candidate.shutdown());
                return Ok(());
            }
            Err(Denied { code: Some(ClientLimitReached), .. })
                if transient_retry && attempt + 1 < attempts => continue,
            // typed actionable failures
        }
    }
}
```

```rust
// Controller additions
SessionControllerCommand::Sync => client.sync()
SessionControllerEvent::AttachFailed { code: Option<AttachDeniedCode>, message }
// every controller outcome remains wrapped in EpochEvent
```

```rust
// production factory
let controller_factory = Arc::new(move || Box::pin(async move {
    let stream = UnixStream::connect(socket).await?;
    let transport = UnixFrontendTransport::new(stream, ..., max_frame)?;
    Ok(spawn_tui_controller(transport, label, limits, socket, reconnect_policy))
}));
TuiOptions { controller_factory: Some(factory), switch_policy: configured }
```

New config: session.switch_attach_retry_attempts=3, switch_attach_retry_backoff_ms=50 with nonzero validation/docs.

Tests all pass:
- switch_commits_whole_candidate_then_detaches_old_controller
- failed_candidate_attach_preserves_current_controller_and_view
- delivered_approval_before_commit_aborts_switch_without_mixing_state
- rapid_switch_back_retries_transient_client_limit
- selecting_current_session_requests_resync_without_new_controller

### Known Concerns
1. Old controller shutdown uses the actor's Shutdown command, which sends Detach and then drops the transport; it is spawned only after whole-instance commit.
2. Final old-event drain narrows but cannot eliminate approval races; daemon eligibility/deadline semantics remain authoritative after commit.
3. This task implements the transaction API and production factory; task 1332's picker will invoke it from UI.

### Specific Questions for Reviewer
1. Does mem::swap plus spawned old shutdown preserve whole-instance atomicity and lifecycle under cancellation?
2. Is last_detached_session single-entry tracking sufficient and correctly scoped for transient cap retry?
3. Can any actor/controller outcome bypass epoch isolation or corrupt the committed candidate?
4. Are admission and final drain correctly placed without introducing mixed state?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Cancellation safety of switch_to is not established. switch_to is an async fn on &mut self; if the future is dropped between factory().await / attach_to(...).await and the mem::swap commit, the candidate controller (already attached to the target session, holding a socket and possibly counting against the daemon client cap) is dropped without an explicit Detach/Shutdown, potentially leaking the attachment or occupying the cap slot until transport teardown is noticed. Specify and test drop/cancellation behavior: either make the candidate a guard type whose Drop spawns shutdown, or document why transport drop is sufficient for prompt daemon-side detach, and add a test for cancellation mid-switch preserving current state and releasing the candidate slot.
B2. mem::swap of the entire TuiSession risks swapping identity/plumbing fields that must NOT move, e.g. any channels registered with external components (event loop senders, terminal/view handles, waker registrations) and last_detached_session bookkeeping. The diff shows candidate.last_detached_session being set before the swap, but after the swap 'self' is the candidate — enumerate every field of TuiSession and justify per-field that swapping is correct, or introduce an inner swappable struct (e.g. ActiveController) so identity fields provably stay put. As written, a stale reconnect factory or event sender pointing at the old instance could silently route events to the detached controller.
B3. TOCTOU between drain_before_switch_commit/ensure_switch_allowed and the commit: attach_to awaits network I/O, and after the final drain there is still an await-free but not race-free window relative to the daemon (a pending approval could be created daemon-side after drain but before commit). known_concerns acknowledges this, but the request must specify the post-commit behavior for an approval that was in flight on the OLD session at commit time: is it surfaced to the user, silently expired by daemon deadline semantics, or lost? At minimum document the contract and assert in the delivered_approval race test what happens to an approval that arrives after drain but before/at commit.
B4. last_detached_session scoping for the ClientLimitReached retry is fragile: (a) transient_retry is computed once before the loop but the drained/allowed state can change between attempts; (b) the field is set on the candidate pre-swap, so after a successful switch A->B, last_detached_session=Some(A) — but if the switch to B FAILS after the old controller already began any teardown, or if two failures interleave, the tracking can go stale and either wrongly enable retries against a still-attached session or wrongly disable the legitimate switch-back case. Define exactly when last_detached_session is written and cleared (including on failed switches and on reconnects), and add a test for retry NOT firing when switching to a third session while at cap.

### Non-Blocking Suggestions
S1. The same-session path (send Sync and return) skips ensure_switch_allowed; confirm that's intentional — resyncing while an approval is pending is probably fine, but note it explicitly since the picker (task 1332) may call this during pending-approval states.
S2. Backoff is a fixed sleep(policy.retry_backoff) per attempt; consider jitter or exponential backoff, since the primary contention case (rapid switch-back at cap=1) races the daemon noticing the old client's detach — 3x50ms may be too tight on loaded systems. Also validate retry_backoff_ms nonzero at config load as stated, and document units in the config key name (already _ms, good).
S3. tokio::spawn(candidate.shutdown()) after swap discards the JoinHandle and any shutdown error; consider logging shutdown failures (e.g. Detach send error) at warn level so orphaned daemon-side clients are diagnosable, especially since they interact with the client cap.
S4. Consider asserting epoch monotonicity in tests: after commit, any late EpochEvent from the old controller epoch should be provably dropped by the event loop; a targeted test injecting a late old-epoch outcome post-swap would directly answer your question 3 rather than relying on inspection.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1333-atomic-switch-implementation/artifacts/round-1-rebuttal.diff" sha256="0ff462f7f1f3bf703839d295c784e2161461a6ab67d27f0f0eba0b285229a247" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — SessionControllerHandle now aborts its owned actor on Drop. Cancellation of attach_to drops the handle, aborts a handshake blocked inside SessionClient::attach, and promptly drops the physical transport. A regression test polls switch_to through candidate Attach/AttachOk (withholds Snapshot), drops the future, proves the current session remains A, and observes candidate transport EOF within one second.
Re B2: ACCEPT — TuiSession now separates stable UI plumbing from `ActiveSession { controller, epoch, state, termination_reported }`. Commit uses mem::replace only on ActiveSession. command_timeout, ControllerFactory, SwitchPolicy, and last_detached_session stay on the original TuiSession identity. No external sender/waker is stored in either structure; each controller internally owns its own epoch-tagged event channel and reconnect factory.
Re B3: CLARIFY — The unavoidable post-drain daemon race is now an explicit ADR contract: approvals delivered before commit abort; an approval created on old daemon state after the final check is not mixed into the new view, loses this client from eligibility after detach, and follows the sticky ineligible deadline; switching back before expiry pauses it. The precommit delivered-approval test proves abort/no mix, and the commit test injects a stale old-epoch whole-state outcome after commit and proves session B remains unchanged.
Re B4: ACCEPT — last_detached_session is written only after candidate hydration, final drain/admission, and active-session replacement have all succeeded; failed/cancelled switches never begin old teardown and never mutate it. &mut self serializes switches, target is immutable, so computing the exact target match once is stable across attempts. Reconnect preserves the same logical session and intentionally does not alter the marker. The marker holds exactly the latest successfully departed logical session. A new test sends ClientLimitReached for unrelated session C while current A has no C marker and proves no second controller/attempt is consumed.

### Revised Code / Diff
```rust
struct TuiSession {
    active: ActiveSession,
    command_timeout: Duration,
    controller_factory: Option<ControllerFactory>,
    switch_policy: SwitchPolicy,
    last_detached_session: Option<String>,
}
struct ActiveSession {
    controller: SessionControllerHandle,
    epoch: u64,
    state: ViewState,
    termination_reported: bool,
}

// commit
let departed_session = self.active.state.session_id().to_string();
let old_active = std::mem::replace(&mut self.active, candidate.active);
self.last_detached_session = Some(departed_session);
tokio::spawn(old_active.controller.shutdown());
```

```rust
impl Drop for SessionControllerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}
```

```rust
// exponential bounded retry
let multiplier = 1_u32 << (attempt - 1).min(31);
sleep(policy.retry_backoff.saturating_mul(multiplier)).await;
```

Tests added/refined:
- cancelling_candidate_handshake_preserves_current_and_releases_transport
- unrelated_client_limit_is_not_retried
- post-commit stale old EpochEvent cannot replace candidate state
- delivered precommit approval aborts and remains visible on current A

ADR-011 now states the late-old-approval eligibility/deadline contract.

### New Points
R1. Same-session Sync intentionally bypasses switching admission: it creates no candidate, detach, or eligibility change and is safe while approvals/turns are active.
R2. Old shutdown is best-effort Detach; the new SessionControllerHandle Drop abort is the transport-close fallback if shutdown itself is cancelled.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. The exponential backoff computes `1_u32 << (attempt - 1).min(31)` — with attempt starting at 0 for the first try, ensure the multiplier expression is only evaluated for attempt >= 1 (the retry sleeps), since (0 - 1) on an unsigned attempt counter would underflow to a huge shift-clamped value; a debug_assert or using attempt.saturating_sub(1) with an explicit skip for attempt 0 would make this robust.
S6. The Drop impl on SessionControllerHandle aborting the actor task means any owner drop (not just switch cancellation) now hard-aborts; verify normal shutdown paths call shutdown() before drop so best-effort Detach isn't skipped in ordinary teardown (e.g. TUI exit), and consider a debug log in Drop when aborting a task that was never asked to shut down.
S7. ADR-011's late-old-approval contract (loses eligibility on detach, sticky ineligible deadline, pauses if switched back before expiry) is daemon-side behavior asserted from the client; if feasible, add or reference a daemon-side test for the switch-back-pauses-approval case so the ADR contract is pinned on both ends rather than only documented.

### Responses to Prior Points
Re R1: Accepted. The same-session Sync path creating no candidate, no detach, and no eligibility change makes bypassing switching admission sound; the rationale that resync is safe during active approvals/turns is convincing and this resolves S1. Recommend the ADR or a code comment capture this so task 1332's picker integration doesn't accidentally re-add an admission gate there.
Re R2: Accepted. Best-effort Detach with the SessionControllerHandle Drop abort as a transport-close fallback gives a clear two-layer teardown story: prompt cooperative detach in the common case, guaranteed transport EOF for the daemon in the cancelled/failed case. Combined with the cancellation regression test observing candidate transport EOF within one second, this closes the lifecycle gap. Per S3, still worth a warn-level log when the spawned shutdown's Detach fails, since orphaned clients interact with the cap.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
