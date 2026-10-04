# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="linear-loop-budget-v2" -->

<!-- event id="request" artifact path="linear-loop-budget-v2-linear-loop-budget-v2/artifacts/round-1-review-request.md" sha256="4e1492f67635516ee76607343ab7f04e448d730404faf152cb0032c85abb48ae" -->
## Review Request — Round 1
**Task:** linear-loop-budget-v2 — Narrow persistent exact-failure budget
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Implement only exact repeated error budget; defer argument-variant grouping and duplicate coalescing. Routing prompt fix already has red-green Rust tests.

### Proposed Plan
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

### Known Concerns
1. Previous re-review generator returned InvalidTransition with unexpected R1/R2/R3 references; fresh thread for revised policy, not claiming approval.
2. Historical wire schema requiredness unavailable; schema mutations gated by reproduced fidelity loss.
3. Opaque errors cannot be confidently classified deterministic.

### Specific Questions for Reviewer
1. Approve this exact-only first guard or identify remaining policy blockers.
2. Is stopping at 128 unique opaque failures too aggressive for legitimate search; suggest safer bounded-state policy if needed.

<!-- event id="linear-loop-budget-review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. The 128 active-key cap is reasonable for legitimate search: unique opaque failures beyond that in a single turn almost certainly indicate a pathological loop rather than productive exploration. To make it safer without losing boundedness, consider evicting least-recently-touched keys at the cap instead of stopping outright (keeping the explicit stop only for repeat-count exhaustion), or emit a warning at a soft threshold (e.g., 96) before the hard stop so the user sees degradation early.
S2. For P1, pin the transient-vs-unknown classification to structured transport error variants (enum/kind), and add a test asserting that an opaque server is_error result whose text resembles a timeout message is still classified unknown, guarding against accidental text-based matching regressions.
S3. In the user-visible budget-stop message, include the fingerprinted tool name and the repeat count so a user can distinguish an intentional identical-retry workflow (and simply start a new turn) from an unnoticed loop.
S4. Document in the detector module that result-hash inclusion in the fingerprint means nondeterministic error payloads (e.g., embedded timestamps or request IDs) will not aggregate; this is acceptable under the exact-only scope but worth a code comment so the deferred variant-grouping work has a clear anchor.

**Verdict:** APPROVE
