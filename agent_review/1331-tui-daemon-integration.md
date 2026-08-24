# Task 1331 — TUI daemon integration review

## Daimonos review request — Round 1
Review this architecture against the live Rust source. Read-only. Inspect agent_runtime, tui/app, headless_frontend, session_daemon, transport/bootstrap, and provider configuration. Find concrete blockers and improvements; end `CONSENSUS: ACCEPT` or `CONSENSUS: REVISE`.

Proposal: full-screen TUI becomes a daemon client, never an AgentSession/provider/tool/persistence owner. Interactive bootstrap ensures/connects to mode-0600 local UDS. A reusable SessionClient controller actor owns transport/attachment and bounded commands (attach/create, prompt, interrupt, approve, set config, list, stop, detach), factoring attach/replay from HeadlessFrontend. Validated snapshots/events flow over bounded channel into ViewState; rendering/input remain TUI. Startup attaches with no ID and renders after AttachOk + snapshot/replay. Detach only disconnects; stop terminates. Reconnect uses ID + sequence. Print mode unchanged. Migrate incrementally then delete local ownership. Tests prove close leaves session alive, reconnect restores view, daemon alone executes/persists.

Identify lifecycle/bootstrap/configuration compatibility, concurrency/backpressure, security, migration, and missing-test problems.

> Cursor alias note: requested `anthropic/claude-fable-5`; CLI exposes this family as `claude-fable-5-high`, used here.

## Claude Fable review — Round 1

Review complete. I read the live source for every module the proposal names: the daemon (`session_daemon.rs`, ~4.2k lines), protocol (`session_protocol.rs`, v2), transport (`client_transport.rs`), client (`headless_frontend.rs`), reducer (`frontend_state.rs`), the current TUI (`tui/app.rs`), and the wiring (`agent_runtime.rs`, `cli.rs`, `session_factory.rs`, `config.rs`).

## Where the proposal already matches the code

The proposal is essentially the codebase's own declared trajectory — `tui/mod.rs` says the TUI "is a *client* of the daemon-owned session core… a local ACP/UDS client task feeds the reducer in the next slice" (ADR-011). Most infrastructure already exists and is tested:

- **Daemon** — `daimonos session-daemon` is a real runtime mode. `bind_local_socket` (lines 1577–1664) enforces owner-only 0700 parent, 0600 socket, `flock`-guarded single owner, and `verify_peer_owner` checks peer UID per connection. Sessions are created via `AgentSessionFactory`, which owns provider construction, tools, MCP, safety, compaction, and `SessionPersistence` — "daemon alone executes/persists" already holds on this path.
- **Attach/replay/reconnect** — `HeadlessFrontend::attach_inner` implements exactly the flow proposed (Attach/Resume, render only after AttachOk + snapshot-or-replay, seq-gap → SyncRequest). Daemon side has watermark replay (`replay_since`), frame-fitted snapshots, and strictly increasing seq validation.
- **Backpressure** — per-client bounded queue (`event_queue_capacity`, default 256) with `try_send`; a lagging client gets `Revoked` rather than blocking others (lines 813–848, 1377–1389).
- **Lifecycle tests** — the daemon test suite already proves the proposal's headline invariants: `detaching_last_client_keeps_daemon_owned_session_alive`, `attach_existing_session_then_detach_leaves_session_running`, `resume_handshake_replays_from_client_watermark`, `reconnect_receives_equivalent_full_snapshot`, `daemon_shutdown_preserves_persisted_session`.
- **Shared reducer** — `tui/state.rs` is already a re-export of `frontend_state::ViewState`; the TUI renders only from it. `contracts/android/v2/` fixtures pin the wire shape.

So "factor a SessionClient from HeadlessFrontend, make the TUI a client" is the right and intended move. The blockers are in what the proposal leaves unspecified.

## Blockers

**B1 — Protocol gaps for existing TUI commands.** `ClientMessage` has no operation for `/clear` (today: `session.lock().await.clear()` + `clear_transcript`, `tui/app.rs:373`) and no surface for `/usage` (cumulative token/cost totals — `ContextUsage` covers window occupancy only). Migrating without adding these silently drops shipped features. The proposal must either extend the protocol (e.g. a `ClearHistory` command and a usage query/event) or explicitly deprecate the commands.

**B2 — Approval timeout is wrong for an interactive operator.** The daemon's `ApprovalBroker` is built with `approval_timeout_secs` (default **30 s**, `config.rs:495`) and safe-denies on expiry. The current TUI blocks indefinitely on the operator's oneshot. After migration, a human who thinks about a risky `exec` for 31 seconds gets an auto-deny and a failed tool call. The proposal needs a policy: suspend/extend the timeout while a client holding `ApproveOnce` is attached, or a separate interactive timeout.

**B3 — Bootstrap is entirely unwritten, and its hard parts aren't acknowledged.** Nothing today spawns or "ensures" a daemon; `run_agent` interactive goes straight to `tui::run_tui` with a locally built provider. Concrete issues the proposal must specify:
- *Race:* two TUIs auto-spawning concurrently — the flock loser exits `AddrInUse`; the client must retry-connect with backoff. The primitive exists; the client policy doesn't.
- *Stdio:* the daemon `eprintln!`s at startup and runs a stdin consent loop when remote is enabled. A daemon spawned from a raw-mode TUI must be fully detached with redirected fds or it corrupts the screen.
- *Shutdown:* there is no protocol-level daemon shutdown — only SIGTERM/Ctrl-C in `run_session_daemon`. A TUI that spawned a stale daemon (protocol-version AttachDenied) can stop sessions but cannot retire the daemon over the wire. Version skew handling needs a defined path (pid tracking + signal, or a protocol message).
- *Credentials:* the daemon loads `agent.env` at startup and fails without an API key; bootstrap must forward `--agent-env`/`--provider` or the spawn fails invisibly.

**B4 — `--model`/`--provider` flag semantics change silently.** Today `agent -i --model X --provider Y` takes effect directly. As a client, provider selection is daemon-owned and immutable; model maps to `SetConfig("model", …)` but `RuntimeOption::accepts` rejects any model not in the daemon's choice list, which is frozen by `discover_models` at *daemon* startup (`agent_runtime.rs:528`), not at client startup. The proposal must define: reject with a clear error, pass through on spawn, or make the daemon's model option refreshable.

**B5 — Client-side reconnect policy after `Revoked` is unspecified.** The daemon revokes on queue lag, replacement, and stop. `HeadlessFrontend` surfaces `ReceiveOutcome::Revoked` but has no reattach loop. A suspended terminal (Ctrl-Z during a token stream — 256 events fill fast) will be revoked routinely; the SessionClient actor must own automatic Resume-with-watermark, distinguishing lag-revoke (reattach) from stop-revoke (exit). This is the single most likely real-world failure mode for a full-screen client and the proposal only says "reconnect uses ID + sequence".

## Improvements (non-blocking)

- **Input loop restructure:** `run_event_loop` uses blocking `event::poll(50ms)`/`event::read()` on the runtime thread. A client TUI must `select!` fairly across transport events, the render tick, and input — migrate to crossterm `EventStream` or a dedicated input thread feeding the same bounded channel. Worth naming in the plan since it's the largest rewrite inside `tui/app.rs`.
- **Behavior change to document:** today `/quit` aborts the in-flight turn (`turn.abort()`, `app.rs:211-214`); post-migration close/detach leaves it running. Intended, but it's a user-visible reversal that deserves a release note and possibly a confirm prompt while a turn is active.
- **MCP analytics parity:** the TUI passes its analytics store into `agent_mcp::connect`; `session_factory.rs:148` passes `None`. Migration silently drops MCP-call analytics for interactive use.
- **Initial prompt vs. existing session:** `agent -i "task"` attaching to a busy session will get `session_busy`. Define whether the initial prompt queues, errors, or forces a fresh session.
- **`event_queue_capacity` (256) tuning:** per-token `AssistantDelta` frames make 256 a thin buffer during snapshot fitting or terminal resize storms; consider raising the default or coalescing deltas server-side.

## Missing tests (beyond what exists)

Daemon-side coverage is strong; the gaps are client/integration:
1. End-to-end pytest: interactive client attaches over the real UDS, prompts, detaches, reattaches, and the rendered `ViewState` matches — only `test_session_daemon_serves_local_attach_and_cleans_socket` exists today in `tests/test_cli_modes.py`.
2. Revoked-then-auto-resume in the SessionClient (lag-revoke path), asserting no transcript loss or duplication across the resync.
3. Bootstrap races: concurrent spawn (flock loser connects), stale-socket recovery, spawn with missing agent env fails with an actionable message.
4. Approval raced with client disconnect mid-decision (broker eligible-count drop → safe deny) observed from a second attached client.
5. Protocol-version skew: old daemon + new client yields the defined recovery, not just `AttachDenied`.

## Verdict

The target architecture is correct and, unusually, most of it is already built and tested — the daemon, protocol, transport, reducer, and headless client are production-quality. But the proposal as written under-specifies the five blocker areas (protocol gaps for shipped commands, interactive approval timeout, the entire bootstrap lifecycle, model/provider flag semantics, and revoke-reconnect policy), each of which would surface as a user-visible regression during the migration it prescribes.

CONSENSUS: REVISE

## Daimonos response — Round 2

I accept all five blockers as material and revise the solution:

1. Preserve shipped commands before deleting local ownership: add daemon commands for clear-history and cumulative usage (or explicitly mark them unavailable only during an intermediate, non-release migration). Clear must be daemon-authoritative, sequenced, persisted, and snapshot-visible.
2. Add a distinct interactive approval policy. I do not favor making approval unbounded: the daemon remains authoritative and safe-deny on loss of eligible clients. Use a configurable interactive timeout substantially longer than 30 seconds, advertised in approval state, with safe-deny on expiry/disconnect.
3. Define `DaemonBootstrap`: connect first; if absent, spawn a fully detached child with stdin/stdout/stderr redirected to a log/null, forward agent-env/config selection, then retry boundedly through the flock race. Record PID/instance metadata under the owner-only runtime directory. On protocol skew, report an actionable error in v1 rather than automatically killing an unknown daemon; controlled replacement can be a later authenticated admin operation.
4. Preserve CLI semantics: when bootstrap creates the daemon, forward provider/model configuration. When attaching to an existing daemon, provider override is rejected clearly because provider is daemon-owned; model override is applied via SetConfig only if advertised, otherwise fail clearly before sending the initial prompt. Dynamic model refresh is not required for v1.
5. The controller owns reconnect with bounded exponential backoff and Resume watermark. Revocation becomes typed by reason/code: lag/transport loss is resumable; stopped/replaced/auth/protocol errors are terminal or require explicit action. Add delta coalescing or capacity tuning and tests for suspend/lag.

I also accept the input-thread/EventStream restructure, `/quit` behavior documentation/confirmation, MCP analytics parity, and explicit initial-prompt policy (new startup session; submit only after idle snapshot).

One qualification: I reject automatic old-daemon termination on version skew in this task because PID files can be stale and killing a user-owned live daemon is more dangerous than a clear refusal. The bootstrap must diagnose the owning PID/endpoint safely and tell the user how to restart it; an authenticated shutdown/reexec protocol should be a separate design.

With these revisions, are any material blockers left? End with ACCEPT or REVISE.

## Claude Fable review — Round 2

Verification complete. I re-checked each revised item against the live source; four of the five are sound and implementable with the current protocol, but re-reading the approval path exposed a problem — partly of my own making in Round 1.

## Correction to my Round 1 finding (B2)

My Round 1 framing — "a human who thinks about a risky exec for 31 seconds gets an auto-deny" — was wrong. The live broker already implements a better policy than either of us described, and it shipped in the original daemon commit (`30c4461`, 2026-08-08), so it was in the tree during Round 1:

```1725:1760:src/session_core.rs
    let resolution = if let Some(timeout) = broker.timeout {
        let mut ineligible_deadline = None;
        loop {
            let eligibility_changed = broker.eligibility_changed.notified();
            if broker.has_eligible_client(&approval_id) {
                tokio::select! {
                    resolution = &mut receiver => {
                        break resolution.map_err(|_| broker_closed())?;
                    }
                    _ = eligibility_changed => continue,
                }
            } else {
                let deadline = *ineligible_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + timeout);
                // ... deny as "approval_timeout" only after the deadline ...
```

The 30-second deadline runs **only while no eligible client is attached**. While a client holding `ApproveOnce` is connected, the wait is unbounded; on disconnect the eligible counts drop (wired at `session_daemon.rs:405-409` on attach and `:2128-2136` on detach) and the deadline starts. This is tested: `canonical_approval_timeout_pauses_for_eligible_client` and `approval_churn_does_not_extend_first_ineligible_deadline`.

## The remaining material issue: revision 2 contradicts this shipped, tested behavior

Your item 2 says "I do not favor making approval unbounded… use a configurable interactive timeout substantially longer than 30 seconds… with safe-deny on expiry." But approval **is already unbounded while an eligible client is attached**, deliberately and with test coverage, and it already safe-denies on loss of eligible clients — which is the actual safety property you name. Implemented as written, item 2 would replace a strictly better shipped policy with a worse one: an operator who steps away mid-approval for longer than the cap gets denied even though their client is attached and healthy — precisely the class of user-visible regression this review exists to block. It would also require rewriting the two passing tests above to assert the regressed behavior.

The correct scope for item 2 is much smaller:

- Keep pause-while-eligible exactly as shipped. The disconnect/expiry deadline is already configurable (`session.approval_timeout_secs`).
- The only genuinely new work is **advertising deadline state** — e.g. an optional deadline field on `ApprovalRequest` or a status event when a request transitions to the ineligible countdown — so a reattaching client can render it. That's an additive protocol change.
- If you genuinely want a finite while-attached cap, that is a deliberate downgrade of tested semantics and needs its own justification; it should not ride in under "preserve interactive UX."

## Everything else verified against the live code

1. **Clear/usage commands** — confirmed still absent from `ClientMessage`/`SessionEvent` (`session_protocol.rs:50-96, 239-289`). Your requirements (daemon-authoritative, sequenced, persisted, snapshot-visible) are the right ones and fit the existing seq/replay/snapshot model. One implementation note: a clear must be a `SessionEvent` (so resume-with-watermark replays it in order) and must reset the `frontend_state` reducer and appear in the `contracts/android/v2` fixtures — make additions optional/additive to avoid breaking the pinned v2 wire shape.
2. See above — REVISE on this item only.
3. **Bootstrap** — sound. Connect-first, detached spawn with redirected fds, env forwarding, bounded retry through the flock race, and PID metadata in the 0700 runtime dir all match the existing primitives. Your qualification rejecting automatic old-daemon kill is correct and I accept it; a clear refusal is a defined path, and the daemon already returns an actionable versioned `AttachDenied` (`session_daemon.rs:771-776`) for bootstrap to surface.
4. **CLI semantics** — implementable as specified with no protocol change: the snapshot carries `runtime_options` and arrives after `AttachOk` and before any prompt is sent, so "apply model via `SetConfig` only if advertised, else fail before the initial prompt" has the ordering it needs. Provider rejection on attach-to-existing is clean.
5. **Typed revocation** — the daemon currently emits three free-form reasons ("session stopped", "event queue lagged; reconnect for a full snapshot", "attachment replaced by reconnect" — `session_daemon.rs:941, 1383, 1400, 1421, 1431`). Typing by code is right; make it an additive optional `code` field on `Revoked` rather than a breaking shape change, for the same v2-contract reason as above. The resumable/terminal split you propose maps cleanly onto the existing reasons.

Items 1, 3, 4, 5, the accepted improvements, and the initial-prompt policy (new session; submit after idle snapshot) have no remaining blockers. Item 2 is the single remaining material issue — not because the goal is wrong, but because the plan text mandates regressing a shipped, tested invariant that already delivers the goal. Replace item 2 with "keep pause-while-eligible; add deadline advertisement; `approval_timeout_secs` already covers the disconnect case" and I have nothing left to block on.

REVISE

## Daimonos response — Round 3

I accept the correction. I had inferred a global approval deadline from the configuration without re-reading the broker's eligibility-paused timer. The final solution preserves the existing invariant: approval waits indefinitely while at least one eligible client is attached; `approval_timeout_secs` applies only during continuous ineligibility and safe-denies on expiry. This task adds only deadline/countdown advertisement needed by a reconnecting UI; it does not introduce an attached-client cap.

All other Round 2 revisions stand, including the refusal to auto-kill a version-skewed daemon. I consider task 1331's design settled. Confirm consensus or identify a remaining material disagreement.

## Claude Fable review — Round 3

Verified against the live broker. One phrasing in the Round 3 statement doesn't quite match the shipped semantics it commits to preserve, but the intent — "preserve the invariant exactly as shipped" — is agreed, so this is a wording pin, not a disagreement.

The pin: "applies only during **continuous** ineligibility" implies the timer resets when eligibility is regained. The code anchors the deadline **once, at first ineligibility**, and never clears it:

```1726:1738:src/session_core.rs
        let mut ineligible_deadline = None;
        loop {
            let eligibility_changed = broker.eligibility_changed.notified();
            if broker.has_eligible_client(&approval_id) {
                tokio::select! {
                    resolution = &mut receiver => {
                        break resolution.map_err(|_| broker_closed())?;
                    }
                    _ = eligibility_changed => continue,
                }
            } else {
                let deadline = *ineligible_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + timeout);
```

A brief reattach pauses expiry (the deny arm is only selected while ineligible) but does not reset the deadline — that is exactly what `approval_churn_does_not_extend_first_ineligible_deadline` pins. Consequence for the only new work in this task: the advertised deadline/countdown must be the anchored first-ineligible instant, must not be re-issued as extended on reattach, and the UI should render "paused" rather than "cleared" when eligibility returns. Since we agree the shipped tests are the specification, the churn-anchored behavior governs over the word "continuous."

All other items stand as verified in Round 2, including the refusal to auto-kill a version-skewed daemon.

CONSENSUS: ACCEPT
