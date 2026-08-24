# Task 1333 — Atomic daemon session switch review

## Daimonos review request — Round 1
Inspect the live code read-only, especially headless_frontend, client_transport, session_daemon, frontend_state, and protocol. Review this proposed solution for concrete correctness/concurrency/lifecycle issues. End CONSENSUS: ACCEPT or REVISE.

Proposal: implement switch as two-phase client transaction. Controller enters Switching and blocks prompts/config. Keep current connection/view. Open second UDS connection/fresh client instance, attach target, hydrate a fresh off-screen ViewState from matching AttachOk plus validated full snapshot/replay. Commit atomically by replacing active transport/session/reducer and increasing attachment epoch, publish one replacement, then close old connection without stopping session. Any pre-commit failure closes candidate and preserves old state. Tag events/results by epoch to reject stale messages. Same-ID is no-op/resync. Tests cover failures, cancellation, late events, limits, shutdown.

Account for task 1331 consensus: reusable daemon client actor, connect-first bootstrap, typed revoke/reconnect, existing approval eligibility semantics. Find blockers rather than style preferences.

> Model alias: Cursor exposes requested anthropic/claude-fable-5 as claude-fable-5-high.

## Claude Fable review — Round 1

Review complete. I inspected `headless_frontend.rs`, `client_transport.rs`, `frontend_state.rs`, the daemon's attach/serve/drop paths in `session_daemon.rs`, and the approval broker in `session_core.rs`. The overall shape of the proposal (two-phase candidate, commit-by-swap, fail-preserves-old) fits the code well, but I found three concrete correctness issues and several required clarifications.

## Blocking findings

**1. Rapid switch-back races the daemon-side attachment teardown (`DuplicateClient`).**
Attachments are keyed per session by `ClientInfo.id`, and a plain `Attach` uses `replace_existing = false`:

```369:371:src/session_daemon.rs
            if clients.contains_key(&client.id) && !replace_existing {
                return Err(SessionDaemonError::DuplicateClient(client.id));
            }
```

The proposal commits, then closes the old connection asynchronously. The old entry leaves `entry.clients` only when the daemon serve task observes EOF and `AttachedSession::drop` runs. So A→B→A with a stable client id races that teardown and gets a spurious `AttachDenied`. You can't dodge it with `Resume` semantics: `replace_existing = true` fires `replacement_notifier` and revokes the old attachment at attach time — pre-commit — violating the fail-preserves-old invariant (and `HeadlessFrontend` only sends `Resume` when its own `ViewState` already matches the session, which a fresh candidate instance never does). The proposal must mint a per-connection client id (e.g. base id + attachment epoch suffix). Request dedup keys (`client_request_key(client_id, request_id)`) change with the id, but request ids are fresh UUIDs anyway, so nothing breaks.

**2. Candidate connect can die silently at the daemon connection cap — indistinguishable from a crash, and it's the switch that gets there first.**
`serve_unix` guards connections with a semaphore of `max_sessions × max_clients_per_session`; when exhausted the accepted stream is silently dropped (`continue`, no `AttachDenied`), around lines 1506–1512. The switch transiently holds two connections, so at the cap the candidate sees plain EOF (`HeadlessError::Disconnected`) mid-handshake. Pre-commit failure handling already preserves the old state, but the test plan must cover this silent-EOF path specifically — it is not the typed `ClientLimitReached` denial, and the controller needs a user-legible message for it.

**3. Epoch filtering must cover `Revoked` and `Error`, not just events.**
Because each connection is a self-contained `HeadlessFrontend` with its own `ViewState`, stale *events* structurally cannot corrupt the new reducer — the real stale-message hazard is the old actor surfacing `ReceiveOutcome::Revoked` (e.g. "attachment replaced" or "session stopped") or a late `Error` after commit. If the controller interprets those against the *active* session it will tear down the wrong view. State explicitly that epoch tagging applies to every actor→controller outcome, and that post-commit outcomes from the old epoch are dropped.

## Required clarifications (fold into the proposal, no redesign)

- **Commit is a whole-instance swap, not field surgery.** `granted_capabilities` and `attached` live inside `HeadlessFrontend` alongside transport and `ViewState`; `replace_transport` deliberately clears attachment. "Replace transport/session/reducer" should read "replace the active `HeadlessFrontend`/actor."
- **Close the old connection by dropping the actor/transport, not by awaiting a graceful `detach()`.** `FrontendTransport::recv` takes `&mut self`, so an actor parked in `receive()` can't concurrently send `Detach`. EOF-close is fully safe: the daemon serve loop treats `None` as detach, and `AttachedSession::drop` (session_daemon.rs:2103) recomputes approval eligibility and sets `last_detached_at` regardless.
- **Same-ID resync must reuse the existing connection via `SyncRequest`**, which the serve loop fully supports (replay or fitted snapshot). A second `Attach` with the same client id to the same session would hit `DuplicateClient` anyway.
- **Switching away from a session with a pending approval starts its auto-deny clock.** If the leaving client was the only eligible approver, the broker's ineligible-deadline path (session_core.rs:1736–1749) denies after the timeout. That is exactly the 1331 "existing approval eligibility semantics," but the switch flow should at least warn when `pending_approvals` is non-empty, since the old view is being discarded.
- **Candidate snapshots may be truncated.** `fit_snapshot_to_frame` can trim to the frame limit and set `history_truncated`; "validated full snapshot" validation must accept that, matching what `attach_inner` already tolerates.
- **Cancellation mid-handshake is safe as drop-the-candidate**: `HandshakeSessionGuard` rolls back only *generated* sessions; a `Load`-opened target stays resident and falls to idle retention. That's acceptable but should be stated.

Non-issues I verified: a turn in flight on the old session survives the close (prompt tasks are detached, results routed to a dead `event_tx` are dropped, `evict_idle_sessions` skips active turns); approval eligibility counts recompute correctly on both attach and drop; daemon shutdown mid-switch yields a typed `AttachDenied` pre-commit.

CONSENSUS: REVISE — accept the architecture; require the per-connection client id (finding 1), the silent-EOF limit case in the failure/test matrix (finding 2), and epoch scoping over `Revoked`/`Error` outcomes (finding 3).

## Daimonos response — Round 2

I accept all three blockers and clarifications. Revised design:
- Every physical connection/actor gets a unique client instance ID derived from a stable UI process ID plus monotonically increasing connection epoch. Session/request identity never assumes stable ClientInfo.id across reconnections.
- Every actor-to-controller outcome—including events, snapshot, Revoked, Error, EOF, and command results—carries the epoch; non-active epochs are ignored after commit.
- Commit swaps the entire staged frontend/actor handle and its validated view/capabilities, not individual transport fields. The old actor is cancelled/dropped; EOF performs detach.
- Candidate handshake EOF maps to an actionable `connection_capacity_or_daemon_unavailable` message in v1 because the server cannot disambiguate. Add exact cap/EOF test; a future server change can type the denial.
- Same-ID selection sends SyncRequest on the existing actor.
- Accept frame-fitted truncated snapshots if internally valid. Cancelling candidate may leave a loaded target resident for normal idle retention.

For pending approvals, task 1339's already proposed authoritative admission rule rejects switching whenever the current snapshot has pending approvals, so there is no warning-only path in v1. This is stronger and avoids starting the old session's ineligibility timer by an intentional switch. A race remains possible after admission; the serialized controller and final pre-commit check should abort candidate if a current-session approval arrives before commit. If an approval arrives after the commit instant, normal disconnect eligibility semantics apply.

Any material issue left?

## Claude Fable review — Round 2

Verification is complete. I checked the revised design's claims against `session_daemon.rs`, `session_core.rs`, `frontend_state.rs`, and `session_protocol.rs`. Here is my Round 2 ruling.

## Claude Fable review — Round 2

All three Round 1 blockers are correctly resolved, and the pending-approvals admission rule is sound against the live broker. Verified specifics:

- Per-connection instance IDs are protocol-safe: `ClientInfo.id` is a free-form string validated only by byte length, `resolve()` uses the client id purely as `resolved_by` attribution (session_core.rs:1591–1634) with no per-client persisted decisions, and request dedup keys change harmlessly.
- The pending-approvals fallback the response relies on is real and better than stated: the auto-deny timer only ticks while no eligible approver is attached, and it **pauses** when eligibility returns (session_core.rs:1726–1758). So an approval that races past the commit instant is not doomed — switching back to the old session halts the countdown. The first ineligible deadline is sticky across churn, which prevents extension games. `ViewState.pending_approvals` is maintained from both snapshot and events, so the admission rule is client-evaluable.
- Whole-instance swap, EOF-as-detach (`AttachedSession::drop` recomputes eligibility and sets `last_detached_at` guarded by `attachment_id`, session_daemon.rs:2103–2150), SyncRequest resync, and truncated-snapshot acceptance all match the code.

**One material issue remains: the finding-1 race is removed under one key but retypes under another.** Minting per-connection IDs eliminates `DuplicateClient`, but the same teardown window — old attachment lingering in `entry.clients` until the daemon serve task processes EOF — now surfaces through the count check:

```372:375:src/session_daemon.rs
            if !clients.contains_key(&client.id) && clients.len() >= entry.max_clients {
                return Err(SessionDaemonError::ClientLimitReached {
                    max: entry.max_clients,
                });
```

With the default `max_clients_per_session = 4` a rapid A→B→A switch-back leaves at most one zombie entry and passes. But the config validates any value ≥ 1, `max_clients_per_session = 1` is exercised throughout the daemon's own tests, and under that config **every** switch-back races EOF teardown and gets a spurious typed `ClientLimitReached` — the identical window that made finding 1 blocking, just retyped. It is pre-commit and preserves old state, so there is no corruption, but treating it as a hard "session full" denial is wrong in exactly this case. Required: the controller must classify `ClientLimitReached` during candidate attach as possibly-transient (short bounded retry, or a message that distinguishes "may be your own departing attachment"), and the test matrix needs a cap=1 rapid switch-back case alongside the accepted cap/EOF test.

One advisory, non-blocking: to shrink the acknowledged post-admission approval race, the final pre-commit check should non-blockingly drain the old actor's outcome channel before deciding, since an `ApprovalRequested` event can sit delivered-but-unprocessed at the commit instant. The daemon-side fallback makes this safe either way.

CONSENSUS: REVISE — architecture and all Round 2 resolutions accepted; the single remaining requirement is transient-`ClientLimitReached` handling on candidate attach plus its cap=1 switch-back test.

## Daimonos response — Round 3

Accepted. Candidate `ClientLimitReached` is classified as possibly transient when the target matches a session this controller detached from recently. The controller retries with short bounded jitter/backoff while preserving the current attachment; after the bound it reports that the target may still contain a departing attachment rather than claiming a permanent full session. The cap=1 A→B→A case is mandatory. It also drains already-delivered old-epoch outcomes before the final admission/commit check; this reduces, but cannot eliminate, the approval race, whose daemon fallback remains authoritative. No server-side eviction of another client is attempted.

Confirm final consensus or state unresolved disagreement.

## Claude Fable review — Round 3

I reconstructed both prior rounds from the transcripts and checked the Round 3 text against my Round 2 requirements. Every remaining item is satisfied:

- Transient `ClientLimitReached` handling: bounded jitter/backoff retry, old attachment preserved throughout, and the post-bound message says the target may still hold a departing attachment instead of claiming a permanent full session. Scoping the transient classification to targets this controller recently detached from is a sound refinement — that is exactly and only the case where the zombie entry is knowable, and the A→B→A race it covers.
- The cap=1 A→B→A test is mandatory, as required.
- The advisory drain of delivered old-epoch outcomes before the final admission/commit check is adopted with the correct caveat that it narrows but cannot close the approval race, and that the daemon-side auto-deny/eligibility fallback remains authoritative.
- No server-side eviction, consistent with the fail-preserves-old invariant from Round 1.

CONSENSUS: ACCEPT.
