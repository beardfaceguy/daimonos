# ADR-011: Interactive full-screen terminal UI for agent sessions

- **Status:** Accepted (incremental; layers land across Vikunja #1091 phases)
- **Date:** 2026-08-04
- **Amended:** 2026-08-23 (Vikunja #1331 daemon-client migration)
- **Tracking:** Vikunja #1091 (project #183, `daimonos-agent`)
- **Relates to:** ADR-010 (session daemon + remote control, Vikunja #1090),
  Vikunja #955 (`daimonos chat` Reedline REPL — superseded/aliased)

## Context

The local agent experience is line-oriented: `daimonos agent` runs one shot and
exits, and `daimonos chat` (Reedline REPL, #955) prints completed turns and
returns to a prompt between them. Neither offers live streaming, structured
tool activity, in-place approvals, or a session that stays attached.

Separately, the remote-control track (#1090 / ADR-010) already extracted the
pieces a richer client needs:

- `session_protocol.rs` — transport-independent canonical **`SessionEvent`**
  stream, **`SessionSnapshot`**, `ClientMessage`/`ServerMessage`, capabilities
  (`Observe`/`Prompt`/`Interrupt`/`ApproveOnce`/`ApproveAlways`).
- `session_core.rs` — `SessionEventRouter` (session-local monotonic sequence),
  approval brokering, session ownership.
- `client_transport.rs` — framed UDS transport with backpressure/frame limits.

The full-screen TUI is therefore **not a new agent loop**. It is one more
**client of the daemon-owned session**, exactly like the Android remote client.
This ADR records how the TUI is structured so it shares that contract instead of
binding rendering to provider/tool hooks.

## Decision

### 1. Layering (strict, independently testable)

1. **Canonical session events/state** — reuse `session_protocol` as-is. No new
   session semantics are invented for the TUI.
2. **Pure view reducer** (`src/tui/state.rs`, `ViewState`) — a deterministic
   fold of `(seq, SessionEvent)` deltas and canonical `SessionSnapshot`s into a
   render-ready view. **No terminal, no async, no I/O.** Owns *canonical
   ordering only*: duplicate (`seq <= last_seq`) is ignored, a gap
   (`seq > last_seq + 1`) is reported without applying so the client can request
   a snapshot resync, and a snapshot atomically rebases the whole view.
3. **Rendering + input mapping** (ratatui + crossterm) — reads a `ViewState`,
   emits `ClientMessage`s. Retained-mode widgets; never owns session truth.
4. **Local ACP/UDS client task** (`client_transport`) — bounded channels,
   reconnect, snapshot-on-lag; feeds layer 2 and pumps layer 3 output.

Rationale: the same layers 1–2 must serve the Android client (ADR-010), so the
reducer is the shared, portable heart and is the only piece that must be
exhaustively tested for out-of-order/duplicate/reconnect behavior.

### 2. CLI compatibility (explicit, non-negotiable)

- `daimonos agent --interactive` launches the persistent TUI when stdin and
  stdout are attached to a TTY, with an optional initial prompt. Interactive
  mode is deliberately opt-in while session discovery and typed reconnect are
  still being completed.
- A **stable, explicit non-interactive mode** is preserved for scripts, CI,
  benchmarks, and shell composition: `daimonos agent "task"` remains the
  default and `daimonos agent --print "task"` forces it when flags are composed.
  This documented print mode never emits terminal control codes.
- **Non-TTY stdin/stdout falls back to print/line mode automatically** rather
  than emitting raw-mode escapes.
- `daimonos chat` becomes an alias/compatibility fallback; we do **not** maintain
  two divergent interactive agent implementations. Reedline may remain the
  non-full-screen fallback editor but must never own stdin concurrently with a
  raw-mode TUI.

### 3. Detach vs stop contract

The daemon-owned contract is:

- closing/detaching a client does not kill its daemon session;
- reattaching by session id or picker restores a canonical snapshot;
- `/stop-session` explicitly terminates the session;
- the local TUI retains authority to approve, interrupt, revoke remote clients,
  and stop the session, regardless of remote attachment (ADR-010 arbitration).

The TUI now attaches through the bounded `SessionController` actor. `/quit` and
`/detach` close only that client connection; `/stop-session` sends the explicit
daemon stop command. Canonical snapshots and events are the only source of
rendered state. Commands that gate later work wait for a correlated daemon
`CommandResult`; successful `SetConfig` now guarantees that response after the
runtime option is applied. Startup connects first and, only when the socket is
absent or stale, launches a fully detached session daemon with the selected
workspace, config, provider, model, and agent environment. Concurrent launchers
retry the owner-locked socket for a configured bounded interval; they never kill
a version-skewed daemon. The daemon publishes owner-only PID/version metadata
beside its socket for diagnostics and removes it with the socket. Session
discovery and switching remain separate follow-up tasks, so the current command
starts a fresh daemon-owned session.

`/clear` is also daemon-authoritative: it is rejected during active turns,
persists empty history, and emits a sequenced `ConversationCleared` event so
every attached or replaying frontend resets at the same point. `/usage` reads
typed process-lifetime cumulative usage directly from the daemon without
polluting canonical replay.

### 4. Terminal correctness (hard requirements)

- A RAII terminal guard **always** restores canonical mode, cursor, mouse/paste
  state, and the alternate screen on normal exit, error, Ctrl-C, and via a panic
  hook.
- Handle SIGWINCH/resize, suspend/resume, narrow terminals, Unicode width, large
  paste, and bounded scrollback.
- Tool/model output is **never** allowed to inject terminal control sequences;
  it is sanitized/rendered as escaped text (the reducer stores raw text; the
  render layer sanitizes at draw time).
- No unbounded UI event queue or transcript duplication; lag/reconnect recovers
  from a canonical snapshot (enforced by the reducer's gap handling).

## Phases (map to #1091)

0. **This ADR** — CLI compatibility, future detach/stop contract, event
   projection, and fallback. *(done)*
1. Daemon-owned session core + UDS ACP transport. *(largely pre-landed by
   #1090: `session_core` / `session_protocol` / `client_transport`.)*
2. **Pure view reducer + exhaustive unit tests.** *(this slice)*
3. Streaming assistant output, tool lifecycle cards, interrupt (render layer).
4. Permission modal + local control authority.
5. Session/model/usage/remote-control commands + status bar. *(model, clear,
   and usage are daemon-backed; discovery and remote-control commands remain.)*
6. Polish: bounded scrollback + navigation, prompt history, and no-color
   rendering are implemented; expandable diffs/terminal output, search/copy,
   resize/suspend, and broader accessibility remain.
7. Wire the TUI behind opt-in `--interactive`; retain the default and explicit
   `--print` stable print modes. *(daemon-client wiring landed in task #1331;
   connect-first automatic daemon bootstrap has also landed; typed reconnect
   remains.)* Reconsider a TTY default only after the daemon-owned lifecycle is
   complete.

## Verification gates (TDD)

- **Reducer:** every `SessionEvent` variant, plus duplicate, gap, and
  out-of-order handling, and snapshot rebase. *(landed with phase 2.)*
- **Render:** snapshot tests at normal and narrow terminal sizes.
- **PTY integration:** startup, streaming turn, approval, interrupt, detach,
  reconnect, stop.
- **Safety:** forced error/panic/signal restore terminal state; tool output with
  ANSI/OSC escapes cannot control the host terminal; closing the TUI leaves the
  daemon session alive; local and Android clients render identical canonical
  state after reconnect.

## Consequences

- **Positive:** one session core, N frontends (ADR #884 extended). The reducer
  is shared with Android and is cheap to test. Rendering can iterate without
  risking session correctness. Scriptable print mode is protected by contract.
- **Negative / cost:** adds `ratatui`/`crossterm` to the dependency tree (render
  layer only); raw-mode correctness is genuinely fiddly and demands the RAII
  guard + PTY tests; `daimonos chat`'s behavior changes (mitigated by keeping an
  explicit fallback).
- **Deferred:** attachments/file upload, configurable keymaps, and the E2E relay
  remain ADR-010/#1090 future work.
