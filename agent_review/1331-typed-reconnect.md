# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1331-typed-reconnect-20260824" -->

<!-- event id="request" artifact path="1331-typed-reconnect/artifacts/round-1-review-request.diff" sha256="b205802a0abbd44233f1c4c9e712c474a699d15a1dc135ae2e9c9531ed12f752" -->
## Review Request — Round 1
**Task:** 1331 — Typed daemon revocation and automatic TUI reconnect
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Add optional typed RevocationCode to protocol-v2 Revoked while preserving legacy reason-only decode. Codes distinguish SessionStopped, EventQueueLagged, and AttachmentReplaced. Extend SessionController with a typed reconnect policy/factory: resumable EventQueueLagged and transport loss create a fresh Unix transport, call SessionClient Resume with preserved session ID/watermark/client identity, accept replay or snapshot through existing attach validation, and emit Attached only after success. Terminal or untyped revocations surface and stop. Retry count and exponential backoff are configurable/bounded. TUI receives actionable typed terminal errors; Android decodes the optional code and uses SessionStopped without free-text parsing (legacy fallback retained).

### Relevant Code / Diff
```rust
// src/session_protocol.rs
pub enum ServerMessage {
    Revoked {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<RevocationCode>,
        reason: String,
    },
}
#[serde(rename_all = "snake_case")]
pub enum RevocationCode { SessionStopped, EventQueueLagged, AttachmentReplaced }
```

```rust
// src/session_controller.rs
pub struct ReconnectPolicy {
    pub attempts: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}
pub type ReconnectFactory<T> = Arc<dyn Fn() -> ReconnectFuture<T> + Send + Sync>;

async fn reconnect_client<T: FrontendTransport>(
    client: &mut SessionClient<T>, policy: ReconnectPolicy, factory: &ReconnectFactory<T>,
) -> Result<(), SessionClientError> {
    let session_id = client.state().session_id().to_string();
    let mut delay = policy.initial_backoff;
    let mut last_error = "no reconnect attempt completed".to_string();
    for _ in 0..policy.attempts.max(1) {
        tokio::time::sleep(delay).await;
        match factory().await {
            Ok(transport) => {
                client.replace_transport(transport);
                match client.attach(Some(session_id.clone())).await {
                    Ok(()) => return Ok(()),
                    Err(error) => last_error = error.to_string(),
                }
            }
            Err(error) => last_error = error.to_string(),
        }
        delay = delay.saturating_mul(2).min(policy.max_backoff);
    }
    Err(SessionClientError::Protocol(format!("reconnect exhausted: {last_error}")))
}

// run_controller receive branch
Ok(Revoked { code: Some(EventQueueLagged), .. }) if reconnect.is_some() => {
    reconnect_client(...).await?;
    emit(Attached { canonical state, granted capabilities });
}
Ok(Revoked { code, reason }) => {
    emit(StateChanged { outcome: Revoked { code, reason } });
    break ControllerStopReason::Revoked(reason);
}
Err(receive_error) if reconnect.is_some() => {
    match reconnect_client(...) {
        Ok => emit(Attached),
        Err => emit Failed("reconnect") and stop ReconnectExhausted,
    }
}
```

```rust
// src/agent_runtime.rs
let reconnect_factory: ReconnectFactory<UnixFrontendTransport> = Arc::new(move || {
    let socket = reconnect_socket.clone();
    Box::pin(async move {
        let stream = UnixStream::connect(&socket).await.map_err(TransportError::Io)?;
        UnixFrontendTransport::new(stream, label, max_frame)
            .map_err(SessionClientError::from)
    })
});
SessionControllerHandle::spawn_with_reconnect(
    initial_transport, client_info, capabilities, scrollback,
    command_capacity, event_capacity,
    ReconnectPolicy {
        attempts: cfg.session.reconnect_attempts,
        initial_backoff: Duration::from_millis(cfg.session.reconnect_initial_backoff_ms),
        max_backoff: Duration::from_millis(cfg.session.reconnect_max_backoff_ms),
    },
    reconnect_factory,
)
```

```rust
// src/session_client.rs
ServerMessage::Revoked { code, reason } => {
    self.attached = false;
    Ok(SessionClientOutcome::Revoked { code, reason })
}
// attach(Some(same_session)) already emits Resume with state.last_seq and validates replay/snapshot.
```

Daemon Revoked send sites now attach exact codes. TuiSession maps terminal Revoked to Failed; resumable lag is suppressed inside controller until Attached. Android adds nullable RevocationCode and typed SessionStopped handling with reason fallback for old daemons.

Config additions, positive/coherent validation, defaults/docs:
- reconnect_attempts=4
- reconnect_initial_backoff_ms=100
- reconnect_max_backoff_ms=1000

Tests:
- exact typed wire shape + legacy reason-only decode
- lag revocation creates new transport, sends Resume with same client/session and watermark, and emits Attached
- transport EOF reconnects instead of stopping controller
- terminal revocation surfaces typed reason to TUI
- daemon SessionStopped Revoked carries typed code
- Android typed/legacy decoding test

Focused Rust tests and cargo check pass.

### Known Concerns
1. Missing code from an old daemon is treated as terminal; no free-text parsing is used to infer resumability.
2. Reconnect temporarily blocks the single controller actor, but command input remains bounded and total delay is limited by configured attempts/backoff.
3. Android keeps its existing outer reconnect loop; this new factory is only for the local Unix TUI controller.

### Specific Questions for Reviewer
1. Can lag revocation or transport EOF lose canonical state/commands across Resume?
2. Are the chosen terminal/resumable classifications correct, especially AttachmentReplaced?
3. Can shutdown or queued commands deadlock while the controller is inside bounded reconnect?
4. Does reconnect need a new epoch/client ID, or is retaining logical client identity correct for Resume replacement semantics?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. State-loss risk across Resume is not demonstrated (question 1). The design accepts 'replay or snapshot through existing attach validation', but on EventQueueLagged the daemon has, by definition, dropped events past the client's watermark, so replay from state.last_seq may be impossible and a snapshot path is mandatory. The proposal needs an explicit test showing that when replay from the preserved watermark is unavailable, Resume falls back to a snapshot that fully re-establishes canonical state (scrollback, cursor, capabilities), and that the emitted Attached reflects the snapshot rather than stale controller state. Also specify what happens to commands the client sent after the daemon revoked but before the client observed Revoked: are they acknowledged, dropped, or silently lost? If they can be silently lost, that must at least be documented and surfaced to the TUI.
B2. Reconnect blocking the single controller actor creates a shutdown/deadlock hazard (question 3). With attempts=4 and backoff 100..1000ms plus connect/attach latency, the controller can be unresponsive for multiple seconds while awaiting reconnect_client. During that window it processes no commands, including shutdown/detach. Bounded command channels can fill and block senders; if any sender holds a resource the reconnect path needs (or the TUI blocks on send during teardown), you get a stall or deadlock until retries exhaust. Require either (a) reconnect performed with a select! against the command channel so shutdown/detach cancels reconnect immediately, or (b) an explicit test proving shutdown completes promptly while the controller is mid-backoff and that queued command senders cannot deadlock teardown.
B3. AttachmentReplaced classification and reconnect-loop safety are unresolved (question 2). Treating AttachmentReplaced as terminal is likely correct for the revoked client, but the interaction with the reconnect path is dangerous: if a transport-level error (Err(receive_error) branch) is caused by another client attaching and replacing this one, the controller will reconnect and attach(Some(session_id)), which per 'attach validation' presumably replaces the new attachment—two clients with reconnect enabled can then ping-pong, each revoking the other in a livelock. Specify daemon-side semantics for Resume vs. replacement (e.g., Resume with a stale watermark/older client identity does not steal an active attachment, or replacement revocations are delivered before transport close so the Err branch cannot mask them) and add a test covering the replaced-then-transport-error ordering.

### Non-Blocking Suggestions
S1. Client identity across reconnect (question 4): retaining the logical client identity for Resume seems correct given the daemon keys resumption on session ID + watermark, but consider adding a monotonically increasing attach epoch in the Resume message so the daemon can deterministically reject stale/duplicate resumes from a half-dead prior transport. Not blocking if the daemon already invalidates the old transport on new attach.
S2. The first reconnect attempt sleeps initial_backoff before trying; for transport EOF (e.g., daemon restart under a supervisor) an immediate first attempt with backoff only on failure would cut typical recovery latency with no downside.
S3. reconnect_exhausted error collapses all attempt failures into the last error string. Consider including the attempt count and, if cheap, distinguishing connect failures from attach/Resume rejections, since the latter (e.g., session gone) should arguably short-circuit remaining retries rather than burn backoff attempts.
S4. Missing-code-as-terminal for old daemons is a reasonable conservative default; document it in the config/docs so operators upgrading the TUI before the daemon understand why lag revocations stop instead of reconnecting.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1331-typed-reconnect/artifacts/round-1-rebuttal.diff" sha256="ace5be2a231041390a5644ffda08d992d4088e6a61a8e7a4f2abe0f72082bf7b" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — The lag test only exercised zero-distance replay. It now sends AttachOk at seq=5 followed by a full snapshot containing recovered transcript state; the controller emits Reconnected only after SessionClient applies that snapshot, and the test asserts state.last_seq=5 plus recovered content. Reconnected is a distinct controller event whose TUI projection adds a persistent local warning that commands accepted immediately before revocation/transport loss may require verification. Commands still in the bounded controller queue are processed after reconnect; only already-written, unconfirmed wire commands are explicitly called out as ambiguous.
Re B2: ACCEPT — Reconnect no longer monopolizes the actor without command service. drive_reconnect pins the reconnect future and select!s it against the command receiver. Shutdown or Detach cancels reconnect immediately; other queued commands receive a bounded Failed('command rejected while reconnecting') event rather than blocking senders. A test enters a permanently-pending reconnect factory with one-second backoff and proves shutdown completes within 100ms.
Re B3: ACCEPT — Explicit AttachmentReplaced remains terminal and has a test proving the reconnect factory is never invoked even when the transport closes immediately after the typed revocation. For the ambiguous EOF-without-revocation case, each physical reconnect now rotates to a fresh UUID client id before Resume. It therefore cannot steal/replace the newer attachment that may have caused the EOF, preventing ping-pong; lag and transport tests assert the new id differs while session id/watermark are preserved.

### Revised Code / Diff
```diff
--- a/src/session_client.rs
+++ b/src/session_client.rs
@@
+pub fn rotate_client_identity(&mut self) {
+    self.client.id = format!("reconnect-{}", Uuid::new_v4());
+}
```

```diff
--- a/src/session_controller.rs
+++ b/src/session_controller.rs
@@
 pub enum SessionControllerEvent {
     Attached { state, granted_capabilities },
+    Reconnected { state, granted_capabilities, message: String },
 }
@@
 async fn reconnect_client(...) {
-    for attempt { sleep(delay); factory; attach; }
+    for attempt {
+        if attempt > 0 { sleep(delay).await; }
+        let transport = factory().await?;
+        client.replace_transport(transport);
+        client.rotate_client_identity();
+        client.attach(Some(session_id.clone())).await?; // replay OR snapshot
+    }
 }
+
+async fn drive_reconnect(..., commands: &mut Receiver<Command>, ...) -> ReconnectDrive {
+    let reconnect = reconnect_client(...);
+    pin!(reconnect);
+    loop {
+        select! {
+            biased;
+            command = commands.recv() => match command {
+                None => Stop(CommandChannelClosed),
+                Some(Shutdown) => Stop(Shutdown),
+                Some(Detach) => { emit(Detached); Stop(Shutdown) },
+                Some(command) => emit(Failed {
+                    operation: command.operation(),
+                    message: "command rejected while reconnecting",
+                }),
+            },
+            result = &mut reconnect => return Connected_or_Failed(result),
+        }
+    }
+}
@@ lag revocation
-reconnect_client(...).await?;
-emit(Attached { ... });
+drive_reconnect(...).await?;
+emit(Reconnected {
+    state: canonical_replayed_or_snapshot_state,
+    message: "session resumed after event queue lag; verify any command accepted immediately before revocation",
+});
@@ transport loss
+emit(Reconnected {
+    message: "session resumed after transport loss; verify any command accepted immediately before disconnect",
+});
```

```diff
--- a/src/tui/session.rs
+++ b/src/tui/session.rs
@@
+SessionControllerEvent::Reconnected { mut state, message, .. } => {
+    state.push_system_message(message);
+    self.state = state;
+    Updated
+}
```

Tests revised/added:
- lag_revocation_resumes_with_watermark_on_new_transport now forces snapshot fallback at seq 5 and asserts recovered transcript/state.
- transport_loss_reconnects_instead_of_stopping_controller asserts fresh physical client id.
- shutdown_cancels_an_inflight_reconnect_promptly proves 100ms shutdown against pending reconnect.
- attachment_replaced_revocation_never_enters_reconnect_loop proves typed terminal classification before EOF and zero factory calls.

Reconnect attempts are now immediate first, with exponential delay only after failure. Config docs will state missing typed codes are terminal for legacy daemons.

### New Points
R1. A fresh physical client ID on every EOF reconnect is the anti-livelock boundary: ambiguous EOF can resume session state but cannot replace another active attachment.
R2. Reconnected is intentionally distinct from Attached so recovered canonical state and uncertainty about pre-disconnect wire commands are both surfaced without polluting daemon replay.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2_retry" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S5. The rotated physical client id uses a 'reconnect-{uuid}' prefix; consider preserving the original human-readable label as a prefix (e.g., 'tui-<orig>-reconnect-<uuid>') so daemon logs can correlate reconnected clients with their original attachment for debugging.
S6. drive_reconnect rejects non-shutdown commands with Failed('command rejected while reconnecting'). Consider whether purely local/read-only commands (if any exist, e.g., viewport scroll) should be serviced rather than rejected; if none exist today, a comment noting the rejection is intentionally blanket would prevent future regressions.

### Responses to Prior Points
Re R1: Agreed. Rotating to a fresh physical client id on each EOF reconnect is a sound anti-livelock boundary: the ambiguous EOF path can no longer masquerade as the prior attachment and steal back a replacement, and the explicit AttachmentReplaced terminal test plus the zero-factory-invocation assertion close the ordering gap I raised in B3. The retained session id + watermark preserves resumption semantics, which also addresses S1 adequately (identity rotation serves the same stale-resume-rejection purpose as an epoch).
Re R2: Agreed. A distinct Reconnected event is the right shape: it lets the TUI surface both recovered canonical state and the pre-disconnect command ambiguity as a persistent local warning without inventing daemon-side replay semantics. Combined with the forced snapshot-fallback test asserting seq=5 and recovered transcript, this satisfies B1's demonstration requirement, and the documented ambiguity for already-written unconfirmed wire commands is an acceptable, honest resolution.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
