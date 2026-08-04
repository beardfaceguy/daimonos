# ADR-011: Interactive full-screen terminal UI for agent sessions

- **Status:** Accepted (incremental; layers land across Vikunja #1091 phases)
- **Date:** 2026-08-04
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

- `daimonos agent` becomes an **interactive persistent TUI when attached to a
  TTY**, with an optional initial prompt.
- A **stable, explicit non-interactive mode** is preserved for scripts, CI,
  benchmarks, and shell composition (e.g. `daimonos agent --print "task"`). The
  exact flag set is finalized in the phase-7 slice; the invariant is that a
  documented, scriptable print/line mode always exists and never emits terminal
  control codes.
- **Non-TTY stdin/stdout falls back to print/line mode automatically** rather
  than emitting raw-mode escapes.
- `daimonos chat` becomes an alias/compatibility fallback; we do **not** maintain
  two divergent interactive agent implementations. Reedline may remain the
  non-full-screen fallback editor but must never own stdin concurrently with a
  raw-mode TUI.

### 3. Detach vs stop (daemon-owned session)

- The session is **daemon-owned**: closing/detaching the TUI does not kill it.
- `/detach` (or closing the terminal) leaves the daemon session alive and
  reconnectable; `/stop-session` explicitly terminates it.
- The local TUI retains authority to approve, interrupt, revoke remote clients,
  and stop the session, regardless of remote attachment (ADR-010 arbitration).

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

0. **This ADR** — CLI compat, detach/stop, event projection, fallback. *(done)*
1. Daemon-owned session core + UDS ACP transport. *(largely pre-landed by
   #1090: `session_core` / `session_protocol` / `client_transport`.)*
2. **Pure view reducer + exhaustive unit tests.** *(this slice)*
3. Streaming assistant output, tool lifecycle cards, interrupt (render layer).
4. Permission modal + local control authority.
5. Session/model/usage/remote-control commands + status bar.
6. Polish: expandable diffs/terminal output, search/copy, resize/suspend,
   accessibility (no-color, keyboard-only).
7. Make the interactive TUI the TTY default; retain the explicit stable print
   mode.

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
