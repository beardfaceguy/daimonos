# Task 1339 — Switch admission review

## Daimonos request — Round 1
Inspect live daemon/client/reducer approval and operation state. Review: `/load_conversation` switch is allowed only when canonical turn is Idle, no pending approvals, and no prompt/interrupt/config/stop/sync/reconnect/switch operation is in flight. Enter rechecks in serialized controller; set Switching before await; reject new prompt/config until commit/rollback. UI disabling is advisory. Stable rejection codes. No auto-interrupt/deny/approval transfer. Integrate task 1333 consensus: staged second connection, whole actor swap, epoch-tagged outcomes, drain delivered old outcomes immediately pre-commit, unavoidable later races use daemon semantics. Find material race/state gaps and tests. End CONSENSUS: ACCEPT or REVISE.

> Cursor model alias: claude-fable-5-high.

## Claude Fable — Round 1

I've inspected the live stack end to end: the daemon dispatch loop and attach path in `session_daemon.rs`, the turn controller and approval broker in `session_core.rs`, the client controller in `headless_frontend.rs`, and the shared reducer in `frontend_state.rs`. The proposed admission policy is directionally sound and fits the live primitives, but I found material gaps that need revision before implementation. Verdict at the end.

## What the live code confirms is right about the design

**Daemon already owns real admission; client checks are correctly advisory.** Per-session serialization exists on the daemon: the attach handshake holds an owned `admission` mutex until `finish_handshake` (lines 358 and 905–906 of `src/session_daemon.rs`), prompt admission takes the same mutex around `begin_turn()` (lines 1010–1018), and `SetConfig` takes it too (line 1271). Cross-client turn exclusivity is `TurnController::begin` (`session_core.rs` line 1360). So "UI disabling is advisory, enforcement in the serialized controller, and unavoidable later races use daemon semantics" matches reality.

**No approval transfer or auto-deny is consistent with the broker.** Approvals live in the session-owned `ApprovalBroker`, keyed by broker-minted monotonic ids, and survive client detach. The only automatic deny is the pre-existing ineligibility timeout (`request_approval`, `src/session_core.rs` lines 1736–1757), which fires when approver-eligible client counts (recomputed on attach/detach, `session_daemon.rs` lines 394–409 and 2120–2132) stay at zero past the deadline. A switch that detaches the sole approver triggers that pre-existing semantic, not a new one — acceptable under the stated "daemon semantics" clause, but it should be named in the design as the concrete consequence.

**Whole actor swap + epoch tags + pre-commit drain fits the architecture.** The reducer is a pure sequenced fold with `apply_snapshot` rebasing atomically, and `HeadlessFrontend` already validates that a midstream snapshot cannot change the attached session (test at line 833). Because `receive()` outcomes travel through channels to the UI loop, "drain delivered outcomes pre-commit" plus epoch tags for in-flight ones is the right two-part answer — a drain alone cannot catch frames still in the old transport's buffers.

**Stable code namespace exists to extend.** The daemon already uses stable snake_case codes (`session_busy`, `config_locked`, `capability_denied`, `duplicate_request`, `session_stopped`, …), so client-side `switch_*` codes slot in naturally.

## Material gaps (the REVISE items)

**1. "Canonical turn is Idle" wrongly blocks switching forever after an interrupted turn.** `TurnStatus::Cancelled` is a deliberate *terminal* state distinct from `Idle` — the enum comment says so (`src/session_protocol.rs` lines 146–149), and turn teardown emits exactly one of `Cancelled` or `Idle` (`src/session_core.rs` lines 1286–1292) with nothing ever transitioning `Cancelled → Idle` until a *new* prompt emits `Running`. Under a strict `== Idle` admission check, any user who interrupts a turn can never `/load_conversation` again without first running another prompt to completion. The policy must define idle-equivalence as `{Idle, Cancelled}` (with `Running`, `WaitingForApproval`, `Cancelling` blocking).

**2. The staged second connection's client-id policy is unspecified, and the same-session case diverges dangerously by choice.** `attach_internal` (`src/session_daemon.rs` lines 344–423) rejects a duplicate client id with `DuplicateClient` on the attach path, but the resume path (`replace_existing = true`, lines 377–381) *revokes the existing attachment before the new one is committed* — the old connection gets `Revoked { "attachment replaced by reconnect" }` (line 1428–1435). If the staged connection reuses the TUI's client id and takes the replace path, a switch to the *current* session destroys the rollback target during staging: a subsequent stage failure leaves the user attached to nothing. The design must pick one: (a) mint a connection-scoped unique client id for the staged connection (then also account for `max_clients` slot consumption during the overlap window, lines 372–376), or (b) reject switch-to-current-session outright at admission with its own stable code. Either works; leaving it implicit does not.

**3. "Sync in flight" has no defined clear condition, and the obvious one wedges.** `HeadlessFrontend::receive` fires `SyncRequest` internally on a gap and surfaces `RequestedSync` (lines 248–255), but the daemon may answer with an *event replay* rather than a snapshot (`SessionReplay::Available`, `session_daemon.rs` lines 1189–1200). If the controller sets a sync-in-flight flag on `RequestedSync` and clears it only on `AppliedSnapshot`, replay-based recovery leaves the flag stuck and switching is blocked forever. The clear rule must be: cleared on `AppliedSnapshot` **or** when `last_seq` reaches the `expected_seq` that triggered the sync.

**4. The Switching state names too few rejected inputs.** The policy rejects "new prompt/config until commit/rollback", but the inputs that can mutate control state also include interrupt, approval responses, `/stop-session`, and `/clear`. An `ApprovalRequested` *can* legitimately arrive mid-stage (another attached client prompts the old session after admission passed — nothing prevents that, and ADR-010 is explicitly multi-client). If the user answers it while Switching, the controller must have a defined behavior. Recommendation: during Switching, reject every session-mutating input with `switch_in_progress`, and let the arrived-too-late approval fall to daemon semantics (eligibility timeout) — consistent with "no approval transfer".

**5. Old-connection death during staging has no defined outcome.** The old attachment can be revoked mid-stage independently of the switch: event-queue lag (`session_daemon.rs` lines 1381–1388), session stop, or replacement. "Rollback" then has no live target. The design needs a rule — e.g. if the staged attach ultimately succeeds, commit anyway; if it fails, land in a defined detached state with a visible reason, never a silent rollback to a dead actor.

**6. Daemon-side stage failures can't produce stable codes today.** `AttachDenied` carries only a free-text `reason` (`src/session_protocol.rs` lines 107–109), and `ClientMessage::Attach` has no `request_id`. The 1339 deliverable requires user-visible rejection reasons with stable codes; staged-attach failures (session not found, stopped, client limit, snapshot too large) are exactly the ones users will hit. Either add a `code` field to `AttachDenied` (protocol bump — version field exists, currently 2) or explicitly scope stable codes to client-local rejections and accept free text for daemon denials. The design should say which.

## Test gaps

Existing coverage is good at the layer boundaries (reducer ordering/rebase; frontend attach-failure rollback, midstream-snapshot rejection, reconnect resume; daemon replacement revocation, busy/config-locked, approval eligibility timeout). Missing for this work:

- Admission matrix: each blocking state (Running, WaitingForApproval, Cancelling, pending approval present, each in-flight op kind) × switch attempt → the exact stable code; plus the allowed states, **including `Cancelled`** (regression for gap 1).
- Switching-before-await: stage suspended (in-memory transport withholds `AttachOk`), then prompt/config/interrupt/approval each rejected with `switch_in_progress` until commit or rollback.
- Rollback integrity: stage fails via `AttachDenied`, post-`AttachOk` error, and transport drop → old actor intact, old `ViewState` byte-identical, next prompt on the old session works.
- Epoch semantics: delivered old-session outcome drained pre-commit is applied; an old-epoch outcome arriving post-commit is dropped; a `CommandResult` for a pre-switch request never reaches the new epoch.
- Same-session switch under whichever policy gap 2 selects, including the `max_clients`-exhausted staged attach.
- Cross-client race: second client prompts the old session after admission passes → switch still commits, old session left mid-turn, pending approval resolves by daemon timeout — asserting the switch itself never emitted a deny.
- Sync-in-flight lifecycle: gap → blocked; replay-based (snapshot-less) recovery clears the flag → allowed (regression for gap 3).
- Old connection `Revoked` mid-stage → defined outcome per gap 5.

## CONSENSUS: REVISE

The skeleton — serialized controller recheck, Switching-before-await, advisory UI, staged connection with whole-actor swap, epochs plus pre-commit drain, daemon semantics for late races — is correct against the live code and should stand. Revision is needed on six points before implementation, the two hard blockers being the `Idle`-vs-`Cancelled` admission predicate (a user-facing permanent wedge) and the staged connection's client-id/same-session rule (a rollback-destroying interaction with the live replacement path).

## Daimonos response — Round 2

I accept all six corrections, with integration from task 1333:
1. Quiescent turn states are `Idle | Cancelled`; Running, WaitingForApproval, and Cancelling block. Pending approvals independently block even if status is stale/quiescent.
2. Task 1333 already settled unique per-connection IDs and same-ID SyncRequest on the existing actor; same-ID never stages Resume/replacement. Candidate overlap accounts for limits and transient cap=1 teardown retries.
3. Track sync target sequence. Sync clears on an applied snapshot or when ordered replay advances last_seq to at least that target; disconnect/error clears into reconnect/error state, never idle.
4. Switching rejects every session-mutating command: prompt, config, interrupt, approve/deny, stop, clear, and another switch. Read-only local UI operations remain available. Late approval uses daemon eligibility semantics.
5. Old actor liveness is tracked during staging. If candidate succeeds, commit. If candidate fails and old actor is dead, enter an explicit Detached/Error state with the old revoke/disconnect reason and offer reconnect; never claim rollback succeeded.
6. Client-local admission uses stable `switch_*` codes now. Daemon attach denial codes are required by task 1335 and should be an additive optional protocol field; until that lands, preserve free text under generic `switch_attach_denied` without parsing it.

I accept the complete proposed test matrix. Any material issue remains?

## Claude Fable — Round 2

I've verified everything needed. All context reconstructed: Round 1 findings for 1339, the full 1333 consensus (which ended ACCEPT at Round 3), the live protocol/daemon/frontend code, and task 1335's actual scope in Vikunja.

## Claude Fable review — Round 2

Verified against the live code and the 1333 consensus record:

- **Point 1** matches `TurnStatus` exactly (`src/session_protocol.rs:141–149`): quiescent = `{Idle, Cancelled}`, with the independent pending-approvals gate. Correct, and the independence clause properly covers stale-status windows since `ViewState.pending_approvals` is maintained from both snapshot and events.
- **Point 2** accurately restates the 1333 final consensus (unique per-connection IDs, same-ID → `SyncRequest` on the existing actor, never Resume; cap=1 transient `ClientLimitReached` retry with bounded backoff). Consistent with the record.
- **Point 3** is the correct clear rule. `ReceiveOutcome::RequestedSync { expected_seq }` (`src/headless_frontend.rs:54–56`) surfaces the target, and replay events flow through the normal `apply_event` path advancing `last_seq`, so "applied snapshot OR ordered replay reaches target" is implementable as stated. Advisory, non-blocking: a second gap can fire while a sync is outstanding — the tracked target must take the max of successive `expected_seq` values, not the first.
- **Point 4** and **Point 5** resolve gaps 4 and 5 as required. No issues.
- **Point 6**'s premise checks out: task 1335's deliverable is "typed privacy-safe failures … from store through daemon protocol to TUI," so daemon denial codes have a legitimate landing spot there, and an additive optional `code` field on `AttachDenied` is protocol-compatible.

**One material issue remains: points 2 and 6 contradict each other, and the live wire format is why.** Every daemon-side denial reaches the client as `AttachDenied { reason }` — free text only, produced by `session_daemon_error_message`:

```2059:2059:src/session_daemon.rs
        SessionDaemonError::ClientLimitReached { .. } => "client limit reached",
```

The 1333-accepted transient-retry rule (reaffirmed in point 2) requires the controller to classify a candidate denial *as `ClientLimitReached`* before retrying — the post-bound "may still contain a departing attachment" message is only correct for that denial, not for "session has stopped" or "session was not found." But point 6 forbids parsing the free text until the 1335 code field lands. Under that rule the client cannot distinguish `ClientLimitReached` from any other denial, so the mandatory cap=1 A→B→A retry cannot be implemented as accepted, and "retry any denial on a recently-detached target" would emit the departing-attachment message for genuinely permanent failures.

Pick one interim resolution:

- **(a) Narrow parsing exception**: exact-match the single literal `"client limit reached"` string, pinned by a test against `session_daemon_error_message`, scoped to recently-detached targets, removed when the 1335 code field lands. Degradation under daemon/client version skew is safe — an unmatched string falls back to permanent `switch_attach_denied`, which is corruption-free, just retry-less.
- **(b) Sequence the 1335 additive `code` field before or with this work**, so the retry classifier reads typed codes from day one. Cleaner, but couples 1339's landing to 1335.

Either is acceptable; the design must name one. Everything else — including the full test matrix — stands as accepted.

CONSENSUS: REVISE — sole remaining item is reconciling the no-parsing rule (point 6) with the transient-`ClientLimitReached` classifier (point 2) via option (a) or (b).

## Daimonos response — Round 3

Choose option (b): sequence the additive typed `AttachDenied.code` portion of task 1335 before task 1333/1339 switching lands. No free-text parsing exception. This slightly changes the earlier priority graph: the small protocol error-code prerequisite moves ahead of switching implementation, while the rest of task 1335 can remain later. `ClientLimitReached` retry keys only off the typed code and recently-detached target. Sync target uses max across successive gaps.

Confirm final consensus or state unresolved disagreement.

## Claude Fable — Round 3

Both facts check out. Consensus is confirmed — here is my final response for the log.

## Claude Fable — Round 3

**Position:** AGREE

Option (b) resolves the sole remaining blocking item, and I verified its two premises against the live code before accepting:

- `AttachDenied` is still `{ reason: String }` at `src/session_protocol.rs:107–109` with `PROTOCOL_VERSION = 2`, and no type in the codebase uses `deny_unknown_fields`. Serde's default tolerant deserialization therefore makes an optional `code` field genuinely additive in **both** skew directions: an old client ignores the unknown field, and a new client sees `None` from an old daemon.
- "Retry keys only off the typed code and recently-detached target" gives the correct degradation under skew: a missing code means no retry and a permanent `switch_attach_denied` fallback — corruption-free, merely retry-less — which is exactly the safety property the interim options were chosen to preserve. The Round 2 contradiction between the no-parsing rule and the `ClientLimitReached` classifier is gone because there is nothing left to parse.
- Sequencing cost is real but small and correctly scoped: only the additive `AttachDenied.code` slice of task 1335 moves ahead of the 1333/1339 switching work; the rest of 1335 stays where it was.
- The sync-target max-across-successive-gaps rule adopts my Round 2 advisory note verbatim. Resolved.

All six Round 1 gaps are now closed with implementable rules, the reordered dependency (1335 code-field slice → 1333/1339) is named, and the full test matrix stands as accepted — it should gain one case: a staged `AttachDenied` arriving **without** a `code` (old-daemon skew) falls back to permanent `switch_attach_denied` with no retry. That is an addition to an accepted matrix, not a new blocking concern.

**CONSENSUS: ACCEPT**

One procedural note for the operator per the protocol: this exchange is concluded at three rounds with consensus reached, so no escalation summary is needed. The worker owns the log file — I have not written to `agent_review/1339-switch-admission.md`; this response should be appended there verbatim by the worker.
