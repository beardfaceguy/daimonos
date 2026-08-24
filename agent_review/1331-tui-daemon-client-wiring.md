# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1331-tui-daemon-client-20260823" -->

<!-- event id="request" artifact path="1331-tui-daemon-client-wiring/artifacts/round-1-review-request.diff" sha256="9515f77130e686f89db0b79ba5ba95d06a753389007e88b6a9719b7dc0818fb4" -->
## Review Request — Round 1
**Task:** 1331 — TUI daemon integration: replace in-process interactive AgentSession ownership
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Replace the TUI-owned provider/AgentSession/tool lifecycle with a bounded SessionController actor over the configured Unix session-daemon socket. TuiSession attaches, accepts only canonical snapshots/events, forwards commands, waits for daemon-confirmed model changes before the initial prompt, and detaches without stopping the daemon session. Add a frontend Unix transport, bounded non-blocking controller event delivery, lifecycle tests, an external PTY test, and update ADR-011. This is one vertical slice; daemon bootstrap, daemon-authoritative clear/usage, and typed revoke/reconnect remain explicitly deferred within open task 1331.

### Relevant Code / Diff
Load-bearing actual code under review (full uncommitted workspace diff is in the named files):

```rust
// src/agent_runtime.rs — interactive branch
if mode == tui::AgentMode::Interactive {
    if let Some(provider) = provider {
        anyhow::bail!(
            "--provider '{provider}' cannot override a running session daemon; start the daemon with that provider"
        );
    }
    let socket_path = cfg.session.resolved_socket_path(workspace);
    let stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .map_err(|error| anyhow::anyhow!(
            "session daemon is unavailable at {}: {error}; start `daimonos --workspace {} session-daemon`",
            socket_path.display(), workspace.display()
        ))?;
    let transport = crate::client_transport::UnixFrontendTransport::new(
        stream,
        format!("session daemon {}", socket_path.display()),
        cfg.session.max_frame_bytes,
    )?;
    let controller = crate::session_controller::SessionControllerHandle::spawn(
        transport,
        crate::session_protocol::ClientInfo {
            id: "tui".to_string(),
            kind: crate::session_protocol::ClientKind::Terminal,
            label: format!("terminal {}", workspace.display()),
        },
        vec![
            ClientCapability::Observe, ClientCapability::Prompt,
            ClientCapability::Configure, ClientCapability::Interrupt,
            ClientCapability::Stop, ClientCapability::ApproveOnce,
            ClientCapability::ApproveAlways,
        ],
        cfg.tui.scrollback_entries,
        cfg.session.event_queue_capacity,
        cfg.session.event_queue_capacity,
    );
    return tui::run_tui(controller, tui::TuiOptions {
        initial_prompt: task,
        no_color,
        model_override: model,
        history_entries: cfg.tui.history_entries,
    }).await;
}
```

```rust
// src/client_transport.rs — frontend side of framed Unix transport
pub struct UnixFrontendTransport {
    reader: UnixReadState,
    writer: OwnedWriteHalf,
    peer_label: String,
    max_frame_bytes: usize,
}

#[async_trait]
impl FrontendTransport for UnixFrontendTransport {
    async fn send(&mut self, message: ClientMessage) -> Result<(), TransportError> {
        let frame = encode_frame(&message, self.max_frame_bytes)?;
        self.writer.write_all(&frame).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<ServerMessage>, TransportError> {
        let Some(frame) = read_frame(&mut self.reader, self.max_frame_bytes).await? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&frame)?))
    }

    fn peer_label(&self) -> &str { &self.peer_label }
}
```

```rust
// src/session_controller.rs — bounded actor
pub struct SessionControllerHandle {
    epoch: u64,
    commands: mpsc::Sender<SessionControllerCommand>,
    events: mpsc::Receiver<EpochEvent>,
    task: JoinHandle<()>,
}

async fn emit(sender: &mpsc::Sender<EpochEvent>, epoch: u64, event: SessionControllerEvent) -> bool {
    sender.try_send(EpochEvent { epoch, event }).is_ok()
}

async fn run_controller<T: FrontendTransport>(
    epoch: u64,
    mut client: SessionClient<T>,
    mut commands: mpsc::Receiver<SessionControllerCommand>,
    events: mpsc::Sender<EpochEvent>,
) {
    loop {
        if client.is_attached() {
            tokio::select! {
                biased;
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    if !handle_command(epoch, &mut client, command, &events).await { break; }
                }
                received = client.receive() => {
                    match received {
                        Ok(outcome) => {
                            let state = client.state().clone();
                            if !emit(&events, epoch, SessionControllerEvent::StateChanged { state, outcome }).await { break; }
                        }
                        Err(error) => {
                            if !emit_failure(&events, epoch, "receive", &error).await { break; }
                            break;
                        }
                    }
                }
            }
        } else {
            let Some(command) = commands.recv().await else { break; };
            if !handle_command(epoch, &mut client, command, &events).await { break; }
        }
    }
    let _ = emit(&events, epoch, SessionControllerEvent::Stopped).await;
}

impl SessionControllerHandle {
    pub async fn shutdown(mut self) {
        let _ = self.commands.send(SessionControllerCommand::Shutdown).await;
        let _ = (&mut self.task).await;
    }
}
```

```rust
// src/tui/session.rs — canonical-state adapter
pub struct TuiSession {
    controller: SessionControllerHandle,
    epoch: u64,
    state: ViewState,
}

impl TuiSession {
    pub async fn attach(mut controller: SessionControllerHandle) -> anyhow::Result<Self> {
        let epoch = controller.epoch();
        controller.send(SessionControllerCommand::Attach { session_id: None }).await
            .map_err(|error| anyhow::anyhow!("failed to queue daemon attach: {error:?}"))?;
        loop {
            let event = controller.recv().await
                .ok_or_else(|| anyhow::anyhow!("session controller stopped during attach"))?;
            if event.epoch != epoch { continue; }
            match event.event {
                SessionControllerEvent::Attached { state, .. } => return Ok(Self { controller, epoch, state }),
                SessionControllerEvent::Failed { message, .. } => anyhow::bail!("daemon attach failed: {message}"),
                SessionControllerEvent::Stopped => anyhow::bail!("session controller stopped during attach"),
                _ => {}
            }
        }
    }

    pub async fn set_config(&mut self, config_id: impl Into<String>, value: RuntimeValue) -> anyhow::Result<()> {
        self.send(SessionControllerCommand::SetConfig { config_id: config_id.into(), value }).await
            .map_err(|error| anyhow::anyhow!("failed to queue config change: {error:?}"))?;
        let mut accepted_request_id = None;
        loop {
            let event = self.controller.recv().await
                .ok_or_else(|| anyhow::anyhow!("controller stopped during config change"))?;
            if event.epoch != self.epoch { continue; }
            match event.event {
                SessionControllerEvent::CommandAccepted { operation: "set_config", request_id: Some(id) } => accepted_request_id = Some(id),
                SessionControllerEvent::StateChanged { state, outcome } => {
                    self.state = state;
                    if let SessionClientOutcome::CommandResult { request_id, operation, .. } = outcome {
                        if operation == "set_config" && accepted_request_id.as_deref() == Some(request_id.as_str()) { return Ok(()); }
                    }
                }
                SessionControllerEvent::Failed { operation: "set_config", message } => anyhow::bail!("daemon rejected config change: {message}"),
                SessionControllerEvent::Detached | SessionControllerEvent::Stopped => anyhow::bail!("controller stopped during config change"),
                SessionControllerEvent::Attached { state, .. } => self.state = state,
                _ => {}
            }
        }
    }

    pub fn poll(&mut self) -> Option<TuiSessionUpdate> {
        match self.controller.try_recv() {
            Ok(event) => Some(self.apply(event)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(TuiSessionUpdate::Stopped),
        }
    }

    pub async fn shutdown(self) { self.controller.shutdown().await; }
}
```

```rust
// src/tui/app.rs — startup ordering and event loop core
pub async fn run(controller: SessionControllerHandle, options: TuiOptions) -> anyhow::Result<()> {
    let mut session = TuiSession::attach(controller).await?;
    if let Some(model) = options.model_override.as_deref() {
        let candidate = RuntimeValue::String(model.to_string());
        let option = session.state().runtime_options().iter().find(|o| o.id == "model")
            .ok_or_else(|| anyhow::anyhow!("the daemon does not advertise a model option"))?;
        if !option.accepts(&candidate) { anyhow::bail!("model '{model}' is not offered by the running session daemon"); }
        session.set_config("model", candidate).await?;
    }
    let initial_prompt = options.initial_prompt.filter(|p| !p.trim().is_empty());
    if let Some(prompt) = initial_prompt.as_ref() {
        if !matches!(session.state().turn_status(), TurnStatus::Idle | TurnStatus::Cancelled) {
            anyhow::bail!("new daemon session was not idle; initial prompt was not sent");
        }
        session.send(SessionControllerCommand::Prompt { text: prompt.clone() }).await?;
    }
    let mut guard = TerminalGuard::enter()?;
    // terminal setup, bounded poll/render/input loop
    let outcome = run_event_loop(/* terminal state */, &mut session).await;
    guard.restore();
    session.shutdown().await;
    outcome
}

// The actual loop drains session.poll(), renders session.state(), and maps:
// prompt -> Prompt; Ctrl-C -> Interrupt; approval keys -> Approve;
// /model -> SetConfig; /quit or /detach -> loop exit then shutdown/Detach;
// /stop-session -> StopSession then shutdown. Clear/usage/compact show an
// explicit deferred-protocol notice rather than mutating local session state.
```

```rust
// src/session_client.rs — preserve request identity for daemon confirmation
pub async fn set_config(&mut self, config_id: impl Into<String>, value: RuntimeValue) -> Result<String, SessionClientError> {
    self.require(ClientCapability::Configure)?;
    let request_id = self.next_request_id("set-config");
    self.transport.send(ClientMessage::SetConfig {
        request_id: Some(request_id.clone()), config_id: config_id.into(), value,
    }).await?;
    Ok(request_id)
}
```

Tests added/updated in the actual code:
- Unix frontend client/server message roundtrip.
- Controller unique epochs, attach, command forwarding, bounded backpressure, and disconnect termination.
- TUI canonical attach, detach wire behavior, prompt/event flow, and config-change wait for CommandResult.
- PTY CLI test verifies interactive mode attempts daemon connection before loading agent env.

Verification already run: cargo fmt --check; cargo clippy --all-targets -- -D warnings; cargo test (1679 tests); pytest tests/ -v (294 passed).

### Known Concerns
1. The task remains intentionally incomplete: daemon bootstrap, daemon-authoritative clear/usage, and typed revoke/reconnect/deadline advertisement are deferred while the Vikunja task stays open.
2. The controller uses try_send for event delivery and terminates when the UI event channel is full; confirm this fail-closed behavior cannot silently strand a daemon session without an actionable TUI indication.
3. The TUI still polls crossterm synchronously every 50ms rather than using EventStream; confirm this is acceptable for the current slice.
4. Shutdown sends Detach after StopSession when /stop-session exits; verify FIFO wire semantics make this harmless and cannot turn an intended stop into detach-only behavior.

### Specific Questions for Reviewer
1. Are there correctness or lifecycle defects in the controller/TuiSession ownership and shutdown paths?
2. Does model override ordering actually guarantee no initial prompt is sent before daemon acceptance?
3. Can bounded event delivery lose a critical state transition in a way the current TUI cannot recover from?
4. Are the explicit deferrals acceptable for this vertical slice, or does any deferred item make this code unsafe to retain?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Silent event loss / stranded UI on backpressure: `emit` uses `sender.try_send(...).is_ok()`, and `run_controller` treats a failed emit as fatal only in some paths — but more importantly, a full events channel causes the controller loop to `break` and terminate. The final `Stopped` event is itself sent via `try_send` into the same full channel, so it can be dropped too. Result: the controller task exits, the daemon session keeps running, and the TUI's only signal is an eventual `TryRecvError::Disconnected` from `poll()` after it drains the backlog — with no diagnostic explaining why. This directly answers your question 3 in the negative: a critical transition (the terminal `Stopped` reason) can be lost. Fix options: (a) reserve capacity for terminal events (e.g., a separate always-deliverable oneshot/watch for controller termination + reason), (b) on overflow, coalesce into a `Lagged{missed}` event delivered via `send().await` before terminating, or (c) at minimum surface a termination reason the TUI renders ("controller stopped: event queue overflow; daemon session still running — reattach with ..."). Also note the `async fn emit` never awaits anything (try_send is sync), which suggests the design intent (awaited delivery for critical events) and implementation diverged.
B2. `set_config` correlation bug can wait forever / mis-correlate: in `TuiSession::set_config`, the loop returns only when a `CommandResult` matches the `accepted_request_id` captured from `CommandAccepted`. If the daemon's `CommandResult` arrives in the same or an earlier `StateChanged` batch than the `CommandAccepted` event (or if `CommandAccepted` is dropped under B1's try_send backpressure), `accepted_request_id` is still `None` and the match `accepted_request_id.as_deref() == Some(request_id.as_str())` never succeeds — the TUI blocks in `attach`-style loop indefinitely with no timeout. Since `SessionClient::set_config` already returns the request_id synchronously, the controller should propagate that request_id in the `CommandAccepted` it emits locally *before* forwarding, or `TuiSession` should obtain the request_id from the command send path rather than from an event race. Additionally, matching on `operation: "set_config"` with string literals in patterns (`SessionControllerEvent::CommandAccepted { operation: "set_config", ... }`) as shown will not compile as a struct pattern unless `operation` is a `&'static str` — if this is illustrative shorthand, confirm the real code matches on a typed operation enum; stringly-typed operation routing here is fragile. Finally, add a timeout to this wait: a daemon that never answers currently hangs the TUI before the terminal guard is even entered (config change happens pre-`TerminalGuard::enter`), leaving the user with a frozen process and no UI.
B3. Model-override race admits the initial prompt against the wrong model in one path: the ordering (set_config awaited before prompt) is correct for the happy path, but `set_config` also mutates `self.state` from interleaved `StateChanged` events while waiting. If the daemon rejects the config change with `Failed`, `run` bails out correctly. However, if the events channel drops the `Failed` event (B1) or the controller stops, `set_config` errors and the prompt is not sent — good. The actual gap: `Attached { state, .. }` inside the `set_config` wait loop replaces state and continues waiting, but a re-attach implies a new server-side context where the pending request_id is no longer meaningful — the loop will then wait forever for a `CommandResult` that will never come. On any `Attached` event mid-wait, the operation should be failed/retried, not silently continued. Please also confirm the idle-check before the initial prompt (`TurnStatus::Idle | TurnStatus::Cancelled`) is evaluated on state that is post-set_config-confirmation, i.e., that `set_config`'s returned state reflects the daemon-confirmed model, not a stale snapshot.
B4. Shutdown/stop ordering (your known concern 4) is not harmless as written: `run` unconditionally calls `session.shutdown()` after the event loop, and shutdown sends `SessionControllerCommand::Shutdown` through the same FIFO command channel. If `/stop-session` maps to `StopSession then shutdown`, the sequence on the wire depends on whether the controller's shutdown path performs a Detach send. If `handle_command(Shutdown)` sends a Detach frame after StopSession was already forwarded, the daemon may process StopSession then Detach — fine — but if StopSession is still queued in the controller's command channel when Shutdown is handled (biased select can interleave with receive arms, and shutdown may drain-or-drop pending commands), the stop can be dropped and the user's intended stop silently becomes detach-only. The diff doesn't show `handle_command`'s Shutdown semantics (drain-then-close vs. immediate close). Requirement: shutdown must drain already-queued commands (or `/stop-session` must await a daemon-confirmed stop result before initiating shutdown), and a test should assert the wire order StopSession→Detach with StopSession actually delivered. If such a test exists in the lifecycle suite, point to it and I'll resolve this.

### Non-Blocking Suggestions
S1. 50ms synchronous crossterm polling is acceptable for this slice, but wrap it in a named constant with a comment referencing the deferred EventStream migration, and confirm the poll interval doesn't starve `session.poll()` draining under high event throughput (drain-to-empty per tick, not one event per tick).
S2. The `--provider` override bail message is good; consider also detecting a stale socket file (connect refused vs. file absent) to give distinct guidance — 'daemon not running' vs. 'stale socket, remove and restart'.
S3. In `TuiSession::attach`, non-matching-epoch events are skipped with `continue`, and unrecognized events fall through `_ => {}` silently. Consider logging/counting these; a stream of unexpected pre-attach events is a useful diagnostic for protocol drift.
S4. The explicit deferrals (daemon bootstrap, daemon-authoritative clear/usage, typed revoke/reconnect) are acceptable for this vertical slice given the deferred-protocol notices in the TUI and the open task tracking — with the caveat that reconnect/reattach semantics interact with B3; keep that coupling in mind when scheduling the deferred work.
S5. `emit` is declared `async` but only calls the synchronous `try_send`; either make it sync or make critical-event delivery genuinely awaited (relates to B1). As written, the `.await` at call sites is misleading about backpressure behavior.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1331-tui-daemon-client-wiring/artifacts/round-1-rebuttal.diff" sha256="25cf72101adc63e6b49c918cd834d91d82fa3bab6c63845616993f8e0c2d6426" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — The event queue could fill, make the actor exit, and drop its final Stopped event. I added an out-of-band durable ControllerStopReason shared by the actor and handle. EventQueueOverflow explicitly says the daemon session remains available to reattach. TuiSession surfaces that reason as a terminal failure, and the TUI exits through normal terminal restoration so the diagnostic reaches stderr. Added controller and TUI tests that force capacity-1 overflow and assert the durable actionable reason.
Re B2: CLARIFY — The proposed CommandResult-before-CommandAccepted race cannot occur in this actor: handle_command awaits the client send and synchronously emits CommandAccepted before run_controller re-enters select and can call client.receive. The string literal pattern is valid because operation is &'static str, as confirmed by compilation and tests. However, the unbounded wait was valid, and review exposed a deeper real defect: the daemon did not emit CommandResult for successful SetConfig at all. I fixed the daemon success response, added configurable session.client_command_timeout_secs (default 10, validated/documented), bounded attach and command-result waits, and retained request-id correlation.
Re B3: ACCEPT — An Attached event now fails the in-flight request instead of replacing state and continuing. Successful SetConfig now returns CommandResult only after apply_runtime_option succeeds. The daemon may project canonical RuntimeOptionsChanged before or after that direct response, so the local view can update just after confirmation, but the model is already applied under daemon admission before the result; the subsequent initial Prompt is therefore processed against the confirmed model. The idle snapshot is safe because SetConfig admission rejects active turns.
Re B4: ACCEPT — Although Tokio mpsc FIFO and the actor's awaited send/flush meant StopSession was written before a queued Shutdown could be handled, local CommandAccepted was not daemon confirmation. I changed /stop-session to exit with TuiExit::Stop, restore the terminal, await a matching daemon CommandResult through TuiSession::stop(), and only then call controller shutdown/Detach. A test asserts StopSession is observed, the stop future remains pending until CommandResult, and Detach is sent afterward.

### Revised Code / Diff
```diff
--- a/src/session_controller.rs
+++ b/src/session_controller.rs
@@
+#[derive(Debug, Clone, PartialEq, Eq)]
+pub enum ControllerStopReason {
+    Shutdown,
+    CommandChannelClosed,
+    EventQueueOverflow,
+    Transport(String),
+}
+
+impl Display for ControllerStopReason {
+    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
+        match self {
+            Self::EventQueueOverflow => f.write_str(
+                "controller event queue overflowed; daemon session remains available to reattach",
+            ),
+            Self::Transport(message) => write!(f, "daemon transport failed: {message}"),
+            // other terminal reasons...
+        }
+    }
+}
@@
 pub struct SessionControllerHandle {
     // existing bounded channels/task...
+    stop_reason: Arc<StdMutex<Option<ControllerStopReason>>>,
 }
@@
-async fn emit(...) -> bool {
+fn emit(...) -> bool {
     sender.try_send(EpochEvent { epoch, event }).is_ok()
 }
@@
-    loop {
+    let reason = 'controller: loop {
         // command/receive select
-        if !emit(...) { break; }
+        if !emit(...) {
+            break 'controller ControllerStopReason::EventQueueOverflow;
+        }
         // transport failure:
+        break 'controller ControllerStopReason::Transport(error.to_string());
     };
+    *stop_reason.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason);
     let _ = emit(&events, epoch, SessionControllerEvent::Stopped);
 }
```

```diff
--- a/src/config.rs
+++ b/src/config.rs
@@
 pub struct SessionRuntimeConfig {
+    /// Maximum wait for a daemon command result in a local frontend.
+    pub client_command_timeout_secs: u64,
 }
@@
+client_command_timeout_secs: 10,
@@
+if self.client_command_timeout_secs == 0 {
+    return Err("session.client_command_timeout_secs must be greater than zero".to_string());
+}
```

```diff
--- a/src/session_daemon.rs
+++ b/src/session_daemon.rs
@@
 if let Err(error) = attachment.core().apply_runtime_option(&config_id, value).await {
     transport.send(&ServerMessage::Error { request_id, code, message }).await?;
+} else if let Some(request_id) = request_id {
+    transport.send(&ServerMessage::CommandResult {
+        request_id,
+        operation: "set_config".to_string(),
+        changed: true,
+    }).await?;
 }
```

```diff
--- a/src/tui/session.rs
+++ b/src/tui/session.rs
@@
 pub struct TuiSession {
     controller: SessionControllerHandle,
     epoch: u64,
     state: ViewState,
+    command_timeout: Duration,
+    termination_reported: bool,
 }
@@
-pub async fn attach(controller: SessionControllerHandle) -> Result<Self> {
+pub async fn attach(controller: SessionControllerHandle, command_timeout: Duration) -> Result<Self> {
+    tokio::time::timeout(command_timeout, async move {
         // attach and wait for canonical snapshot
+    }).await.map_err(|_| anyhow!("timed out waiting for daemon attach"))?
 }
@@
 pub async fn set_config(&mut self, id: impl Into<String>, value: RuntimeValue) -> Result<()> {
-    // unbounded bespoke wait
+    self.request(SessionControllerCommand::SetConfig { config_id: id.into(), value }, "set_config").await
 }
+
+pub async fn stop(&mut self) -> Result<()> {
+    self.request(SessionControllerCommand::StopSession, "stop_session").await
+}
+
+async fn request(&mut self, command: SessionControllerCommand, expected: &'static str) -> Result<()> {
+    self.send(command).await?;
+    tokio::time::timeout(self.command_timeout, async {
+        let mut accepted_request_id = None;
+        loop {
+            let event = self.controller.recv().await
+                .ok_or_else(|| anyhow!(self.controller_stop_message()))?;
+            match event.event {
+                SessionControllerEvent::CommandAccepted { operation, request_id: Some(id) }
+                    if operation == expected => accepted_request_id = Some(id),
+                SessionControllerEvent::StateChanged { state, outcome } => {
+                    self.state = state;
+                    if let SessionClientOutcome::CommandResult { request_id, operation, .. } = outcome {
+                        if operation == expected
+                            && accepted_request_id.as_deref() == Some(request_id.as_str()) {
+                            return Ok(());
+                        }
+                    }
+                }
+                SessionControllerEvent::Attached { .. } => {
+                    bail!("session attachment changed while waiting for {expected}");
+                }
+                SessionControllerEvent::Failed { operation, message } if operation == expected => {
+                    bail!("daemon rejected {expected}: {message}");
+                }
+                SessionControllerEvent::Detached | SessionControllerEvent::Stopped => {
+                    bail!("{}", self.controller_stop_message());
+                }
+                _ => {}
+            }
+        }
+    }).await.map_err(|_| anyhow!("timed out waiting for daemon {expected}"))?
+}
@@
 Err(TryRecvError::Disconnected) => {
+    if !self.termination_reported {
+        self.termination_reported = true;
+        Some(TuiSessionUpdate::Failed(self.controller_stop_message()))
+    } else {
         Some(TuiSessionUpdate::Stopped)
+    }
 }
```

```diff
--- a/src/tui/app.rs
+++ b/src/tui/app.rs
@@
+enum TuiExit { Detach, Stop }
@@
-UiCommand::StopSession => { queue(StopSession); return true; }
+UiCommand::StopSession => return Some(TuiExit::Stop),
@@
 let outcome = run_event_loop(...).await;
 guard.restore();
+let stop_result = match &outcome {
+    Ok(TuiExit::Stop) => session.stop().await,
+    _ => Ok(()),
+};
 session.shutdown().await;
+stop_result?;
```

Tests added: event_overflow_records_a_durable_stop_reason; event_overflow_surfaces_an_actionable_terminal_failure; command_wait_is_bounded_when_daemon_never_answers; stop_is_confirmed_before_shutdown_detaches. Daemon SetConfig test now accepts canonical event/CommandResult scheduling in either order while requiring all outputs. Config default/zero validation updated, plus daimonos.default.toml and docs/configuration.md.

### New Points
R1. CommandAccepted necessarily precedes processing CommandResult in the single controller actor: handle_command completes the transport send and local event emit before run_controller returns to its receive select. Overflow now terminates with an out-of-band reason rather than permitting mis-correlation.
R2. Review uncovered an implementation fact absent from the initial request: successful daemon SetConfig emitted canonical runtime events but no CommandResult. The new success response is the load-bearing fix that makes model override confirmation real rather than merely bounded.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. In app.rs, `session.shutdown().await` runs before `stop_result?` is propagated. If session.stop() failed (timeout or daemon rejection), the shutdown path still sends Detach — which is the right wire behavior — but consider making the eventual error message explicit that the stop was NOT confirmed and the daemon session may still be running, so the user knows reattach/manual stop is required. The durable stop-reason text already models this pattern well for the overflow case.
S7. The `termination_reported` flag makes the Disconnected path emit Failed once then Stopped thereafter. Verify the event loop treats the first Failed(controller_stop_message) as terminal-and-rendered (i.e., it survives guard.restore() to stderr) rather than as a transient status line that is lost when the alternate screen is torn down; the B1 test names suggest this is covered — if event_overflow_surfaces_an_actionable_terminal_failure asserts on post-restore output, this is fully satisfied.
S8. The shared request() helper failing on any mid-wait Attached event is correct for this slice, but when the deferred typed reconnect work lands (per S4/B3 coupling), revisit whether Attached-during-wait should become a typed retryable error so frontends can distinguish 'attachment replaced, safe to retry set_config' from genuine failures. A brief note in the open task 1331 body would preserve this context.
S9. client_command_timeout_secs as a plain u64 with zero-rejection is fine; consider documenting the interaction with attach (which now shares the timeout) — a slow daemon cold-start could plausibly exceed 10s, and the failure message should hint at raising session.client_command_timeout_secs, since attach timeout occurs pre-TUI where the config knob is not discoverable.

### Responses to Prior Points
Re R1: Accepted. The single-actor argument is sound: handle_command completes the transport send and the local CommandAccepted emit before run_controller re-enters its select, so a CommandResult cannot be processed before the corresponding CommandAccepted has been emitted within the same epoch. The remaining failure mode I raised (CommandAccepted dropped by try_send overflow) is now closed differently but adequately: overflow terminates the actor with a durable out-of-band ControllerStopReason::EventQueueOverflow rather than allowing the wait loop to continue against a partially-delivered event stream, and TuiSession surfaces that reason via controller_stop_message() on recv() returning None or Stopped/Disconnected. Correlation can therefore no longer silently mis-resolve; it either succeeds, fails with the stop reason, or times out.
Re R2: Acknowledged, and this is the most valuable outcome of the round: the daemon previously never emitted CommandResult for successful SetConfig, which means the original confirmation-wait was structurally unable to complete on the happy path — a defect the bounded timeout alone would have converted into a guaranteed 10-second stall on every model override. The daemon-side success response plus the order-agnostic daemon test (canonical event and CommandResult accepted in either order, both required) is the correct load-bearing fix. Please ensure the commit message or ADR-011 update notes this daemon protocol change, since other frontends relying on set_config confirmation semantics are affected.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
