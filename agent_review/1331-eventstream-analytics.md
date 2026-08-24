# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1331-eventstream-analytics-20260824" -->

<!-- event id="request" artifact path="1331-eventstream-analytics/artifacts/round-1-review-request.diff" sha256="219d5eb53a4b069fea2347cbcc75c1bb92a45bd6ca327a9139280dbf8875a10d" -->
## Review Request — Round 1
**Task:** 1331 — Nonblocking TUI input and daemon outbound-MCP analytics parity
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Replace synchronous crossterm poll/read on the Tokio runtime thread with EventStream selected alongside SessionController updates and a 50ms render interval. Canonical session updates are consumed immediately without forcing per-token redraw; input mutations render on the next bounded tick. Add async TuiSession::next_update while retaining poll for tests. Enable crossterm event-stream on the already-present 0.29 dependency. Separately carry the daemon's existing AnalyticsStore into AgentSessionFactory and pass it to agent_mcp::connect, restoring the analytics path the process-local TUI previously had.

### Relevant Code / Diff
```rust
// Cargo.toml
crossterm = { version = "0.29", features = ["event-stream"] }
```

```rust
// src/tui/session.rs
pub async fn next_update(&mut self) -> TuiSessionUpdate {
    match self.controller.recv().await {
        Some(event) => self.apply(event),
        None => self.disconnected_update(),
    }
}
```

```rust
// src/tui/app.rs
const RENDER_INTERVAL: Duration = Duration::from_millis(50);

async fn run_event_loop(...) -> Result<TuiExit> {
    let mut terminal_events = EventStream::new();
    let mut render_tick = tokio::time::interval(RENDER_INTERVAL);
    render_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    render_tick.tick().await;
    draw_tui(...)?;
    loop {
        enum LoopInput {
            Session(TuiSessionUpdate),
            Terminal(io::Result<Event>),
            TerminalClosed,
            Render,
        }
        let input = tokio::select! {
            update = session.next_update() => LoopInput::Session(update),
            event = terminal_events.next() => match event {
                Some(event) => LoopInput::Terminal(event),
                None => LoopInput::TerminalClosed,
            },
            _ = render_tick.tick() => LoopInput::Render,
        };
        let event = match input {
            Session(Updated) => continue,
            Session(Failed(message)) => bail!(message),
            Session(Detached | Stopped) => return Ok(Detach),
            Terminal(event) => event?,
            TerminalClosed => bail!("terminal event stream closed"),
            Render => { draw_tui(...)?; continue; }
        };
        // existing pure key/paste/resize handling; no blocking poll/read
    }
}
```

```rust
// src/session_factory.rs
pub struct AgentSessionFactory {
    // existing fields
    analytics: Option<Arc<AnalyticsStore>>,
}

// constructor receives/stores analytics
let agent_mcp = agent_mcp::connect(
    &self.config,
    &native_names,
    self.analytics.clone(),
).await;
```

```rust
// src/agent_runtime.rs session daemon setup
let analytics_store = if cfg.analytics.enabled { AnalyticsStore::new(...).ok().map(Arc::new) } else { None };
let services = build_tool_services(..., analytics_store.clone()).await;
let factory = AgentSessionFactory::new(
    // provider/workspace/config/runtime/persistence
    analytics_store,
    services,
);
```

Existing PTY tests exercise interactive startup, cursor query, `/quit`, detach, and concurrent TUI clients. Existing TUI unit tests cover key mapping/rendering; session controller tests cover high-rate state updates and disconnect. Cargo check passes with one crossterm version.

### Known Concerns
1. Rendering remains synchronous but is bounded to one draw per 50ms tick; only terminal event polling was blocking the runtime thread.
2. A continuous ready session stream competes with the interval in unbiased tokio::select; Tokio randomized fairness should still service render ticks.
3. AnalyticsStore ownership now lasts with AgentSessionFactory/daemon process, matching native ToolServices lifetime.

### Specific Questions for Reviewer
1. Can a continuously ready session update stream starve terminal input or rendering?
2. Does EventStream have cancellation/lifecycle constraints around terminal guard restore or suspend/resume?
3. Is 50ms tick redraw behavior equivalent enough for composer and approval responsiveness?
4. Does passing one AnalyticsStore Arc into every daemon-created outbound MCP client preserve correlation without introducing shutdown/resource leaks?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Starvation risk under sustained session-update load: session.next_update() is likely ready far more often than the render tick or terminal events, and unbiased tokio::select! only gives probabilistic fairness per poll cycle. Because the Session(Updated) arm does `continue` and immediately re-enters select!, a hot update stream can dominate wakeups and visibly delay keystroke handling and redraws. Please make the loop deterministic: e.g., drain session updates non-blockingly (try_recv loop with a bounded batch), or use `biased;` ordering that prioritizes terminal input and render tick over session updates, and add a test or measurement demonstrating input latency remains bounded under a high-rate token stream.
B2. EventStream lifecycle and terminal-guard interaction is unaddressed in the diff. crossterm's EventStream spawns/uses an internal reader; if the loop exits via bail!/early return (Failed, TerminalClosed), the diff does not show that raw mode/alternate screen restoration still runs and that the EventStream is dropped before the terminal guard restores state. Also, suspend/resume (SIGTSTP/SIGCONT) with a live EventStream can leave the terminal in a bad mode. Show that terminal restore is guard/Drop-based (not only on the happy path) and specify behavior on suspend/resume, or explicitly document it as out of scope with rationale.
B3. Input-echo latency regression: previously (blocking poll/read) keystrokes presumably triggered an immediate redraw; now input mutations render on the next 50ms tick. Worst-case ~50ms added latency per keystroke in the composer is perceptible to some users, and approval prompts may feel sluggish. Either render immediately after handling a terminal input event (still coalescing session-update-driven redraws to the tick), or provide evidence that tick-bounded echo is acceptable. The current answer to your own question 3 is asserted, not demonstrated.

### Non-Blocking Suggestions
S1. TerminalClosed => bail! turns a benign terminal EOF/detach (e.g., tmux kill-pane, hangup) into an error path. Consider treating a closed terminal event stream as a detach (Ok(TuiExit::Detach)) rather than a failure, so daemon-attached sessions cleanly detach when the terminal goes away.
S2. For the AnalyticsStore parity change: the diff swallows construction failure via `.ok()`. Log the error at warn level so silently-disabled analytics in the daemon is diagnosable; also confirm AnalyticsStore's Drop/flush semantics are safe when the Arc is held across many outbound MCP clients and the daemon shuts down mid-flight (your question 4). A shutdown flush hook or explicit note that writes are synchronous/append-only would resolve this.
S3. Dirty-flag rendering: instead of unconditionally drawing every 50ms tick, set a `needs_redraw` flag on state mutations and skip the draw on ticks when nothing changed. This avoids constant 20fps redraw work for idle sessions, which matters for battery/CPU when many TUI clients are attached.

**Verdict:** REVISE

<!-- event id="rebuttal" artifact path="1331-eventstream-analytics/artifacts/round-1-rebuttal.diff" sha256="2d71aa86b2c68e69659765a08a24a5d79a728f9b0b2f3410b7615c585c45801d" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — The select is now explicitly biased in deterministic order: terminal EventStream first, render tick second, session update third. A continuously ready session stream therefore cannot win when input or the 50ms tick is ready. Session updates are consumed without redraw; the prioritized tick coalesces high-rate token updates into bounded 20fps rendering.
Re B2: CLARIFY — run_event_loop owns EventStream as a stack local. Every return/bail drops that reader before control returns to run(), which then restores cursor/raw/alternate-screen state through TerminalGuard; guard Drop remains the fallback for earlier errors/panic. TerminalClosed now returns clean Detach instead of error. Full SIGTSTP/SIGCONT re-entry was already an explicit phase-6 TUI-polish deferral in ADR-011 and is unchanged by replacing blocking poll/read.
Re B3: ACCEPT — Every handled terminal event now redraws immediately. Ctrl-C, approval keys, mode switches, and scroll actions draw before their early continue; composer/paste/history/resize paths draw at the bottom of the event branch. Only session-driven redraws remain coalesced to the prioritized 50ms tick, so keystroke echo gains no tick latency.

### Revised Code / Diff
```diff
--- a/src/tui/app.rs
+++ b/src/tui/app.rs
@@
 let input = tokio::select! {
+    biased;
+    event = terminal_events.next() => Terminal(event),
+    _ = render_tick.tick() => Render,
     update = session.next_update() => Session(update),
-    event = terminal_events.next() => Terminal(event),
-    _ = render_tick.tick() => Render,
 };
@@
-TerminalClosed => bail!("terminal event stream closed"),
+TerminalClosed => return Ok(TuiExit::Detach),
@@ terminal input branches
 interrupt(session);
+draw_tui(...)?;
 continue;
@@
 handle_approval_key(...);
+draw_tui(...)?;
 continue;
@@
 // after composer/paste/resize handling
+draw_tui(...)?;
```

```diff
--- a/src/agent_runtime.rs
+++ b/src/agent_runtime.rs
@@
-let analytics_store = AnalyticsStore::new(...).ok().map(Arc::new);
+let analytics_store = match AnalyticsStore::new(...) {
+    Ok(store) => Some(Arc::new(store)),
+    Err(error) => { warn!(event="session_daemon_analytics_init_failed", error); None }
+};
@@
-AgentSessionFactory::new(..., analytics_store, services)
+AgentSessionFactory::new(..., analytics_store.clone(), services)
@@ daemon shutdown
 daemon.shutdown().await;
+if let Some(analytics) = analytics_store {
+    if !analytics.wait_until_quiet(Duration::from_secs(cfg.session.shutdown_grace_secs)).await {
+        warn!(event="session_daemon_analytics_drain_timeout", pending=analytics.pending_writes());
+    }
+}
```

Code comment in run() pins EventStream-drop-before-terminal-restore ordering. Existing phase-6 ADR text continues to identify suspend/resume as deferred. Existing PTY tests exercise normal detach and terminal restoration; full suite will rerun.

### New Points
R1. Biased ordering is intentional input/render priority, not fairness optimization: session updates are lowest priority specifically so token floods cannot starve human control.
R2. One AnalyticsStore Arc now spans native ToolServices and per-session outbound MCP bridges, and daemon shutdown uses the existing wait_until_quiet gate before releasing it.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S4. B1's original ask included a test or measurement demonstrating bounded input latency under a high-rate session-update stream. The biased select makes the property true by construction, so this is no longer blocking, but a regression test (e.g., a loop-level test that floods session updates while asserting a terminal event is handled within one poll cycle) would guard the `biased;` ordering against accidental reordering.
S5. With immediate redraws on every handled terminal input event, rapid key-repeat or large bracketed pastes can trigger a burst of synchronous draws. If profiling ever shows this matters, coalesce input-driven redraws by draining any immediately-ready terminal events (poll EventStream non-blockingly) before drawing once. Not needed for approval; noting for the future.
S6. S3 (dirty-flag rendering to skip idle 50ms redraws) remains open and worthwhile as a follow-up; the prioritized tick currently draws unconditionally even when no state changed.

### Responses to Prior Points
Re R1: Accepted. Biased ordering with terminal input first, render tick second, and session updates last is exactly the deterministic prioritization requested. Because the Session(Updated) arm performs no redraw work and the tick outranks it, a hot token stream cannot delay keystroke handling or bounded rendering. The rationale that this is intentional human-control priority rather than a fairness tweak is sound; consider capturing it in a code comment near the `biased;` block so future refactors don't 'fix' the ordering.
Re R2: Accepted. A single AnalyticsStore Arc spanning native ToolServices and per-session outbound MCP bridges preserves correlation, and gating daemon shutdown on the existing wait_until_quiet with a grace timeout plus a warn on drain timeout addresses the leak/mid-flight-write concern from question 4 and S2. This resolves the shutdown side of the analytics parity change.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
