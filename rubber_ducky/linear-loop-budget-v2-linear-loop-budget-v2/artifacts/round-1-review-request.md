**Objective:** Bound identical failed calls across novelty resets without deduplicating dispatch.

**Steps:**
- P1: Propagate explicit transient classification from remote transport timeout/transport failures; opaque MCP error results remain unknown, not labeled deterministic.
  - Acceptance: Rust tests classify transport timeout/connection failures separately from server is_error.
- P2: Per-turn exact failure fingerprint: tool + complete canonical args + error class + result hash. Unknown and deterministic exact repeats count once per completed batch; transient excluded. Warn at 3 rounds, stop at 6. Matching tool/args success clears its keys; unrelated success/novel calls never reset. Bound state at 128 active keys; overflow stops with explicit budget exhaustion. No fuzzy argument normalization. Reset per turn; preserve existing detector.
  - Acceptance: Recovered trace replay stops by sixth identical-failure round.
  - Acceptance: Tests changed queries, interleaved tools, opaque failures, transient recovery, same-batch duplicate counting, matching/unrelated success, pending nonerror polling, per-turn reset, cap.
- P3: No duplicate-call coalescing; warning-only detection deferred until separately tested. No schema mutation unless mock upstream-to-wire test fails. No historical requiredness claim.
  - Acceptance: Dispatch and correlated result semantics unchanged; no Linear writes.

**Acceptance criteria:**
- Rust tests first and focused checks; scope separate from unrelated workspace changes.
- Clear user-visible budget stop, no automatic replay; no real Linear mutations.

**Risks:**
- Exact opaque-error budget may stop intentional identical failed retries; user can start a new turn.
- Changed arguments evade exact threshold; intentionally narrow scope, bounded memory.
- Transport classification must not rely on brittle result text.