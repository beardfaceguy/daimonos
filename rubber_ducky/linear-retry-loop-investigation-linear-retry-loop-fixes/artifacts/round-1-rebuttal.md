**Objective:** Ship narrow, independently tested anti-loop fixes with no speculative schema conversion or dispatch deduplication.

**Context:** Verified 39 calls, 32 failed listings, six duplicate batches, seven steers. Existing detector resets on novelty. Current optional upstream filter works when omitted; historical wire schema unavailable. Remote script dispatch unsupported.

**Steps:**
- P1: Clarify native-only script batching in prompts, signatures and actionable unknown remote-tool error. Rust tests first for named direct-call guidance and prompt boundary; preserve native fallback and direct remote dispatch.
  - Acceptance: Targeted Rust tests initially fail then pass; existing script tests pass.
- P2: Add persistent exact failure-pair budget alongside existing novelty detector. Key = tool name + canonical complete args + error classification + full error result hash, scoped to one turn. Steer on three distinct rounds, stop on six; duplicate same-round observations count once. Novel operations never reset this budget. Genuine success with identical tool/args clears that operation's keys. Different args remain distinct: no dropping query/filter/cursor fields and no fuzzy matching. Explicit transient outcomes (timeouts/transport/rate limits) excluded when classified; opaque remote errors use exact-repeat fallback, disclosed as no-progress guard rather than proof of determinism. Store bounded keys (max 128); overflow ends turn with clear budget-exhaustion message instead of eviction allowing evasion. Clear all state per turn. Keep old detector behavior for other observations.
  - Acceptance: Red-green recovered trace replay stops no later than sixth repeated exact-error round despite novel lookups.
  - Acceptance: Tests for changed args, transient exclusion, opaque exact failures, interleaved tools, matching success reset, unrelated success nonreset, same-batch duplicates, polling is_error=false, per-turn reset and bounded memory.
  - Acceptance: Typed transport classification must be propagated or scope limited/documented before enforcement.
- P3: Warning-only duplicate detection: no coalescing or result caching, no dispatch semantic change. Defer dispatch deduplication to future separate review with trustworthy safety metadata.
  - Acceptance: Recovered duplicate batches detected; all original IDs receive separate results, including writes and cancellation.
- P4: Mock upstream-to-provider-wire schema fidelity tests: optional customView minLength 1, omitted argument, additionalProperties false, nested nullable union. Implement repair only for reproduced loss after review of that repair; no schema mutation if tests green. Historical requiredness remains unverifiable.
  - Acceptance: Tests prove any loss before repair; omission preserved; no real Linear writes.
- P5: Bounded opt-in sanitized canonical schema hashes and correlation diagnostics, explicit cancellation initiator/reason; exclude full provider bodies, arguments and credentials.
  - Acceptance: Canonical hashes independent of key order; redaction/retention/disabled-mode/cancellation-source tests.

**Acceptance criteria:**
- Rust red-green tests for all Rust features; focused tests, fmt, clippy and available full suites with caveats.
- Independent commits P1 then P2 then tests-only P4, P5, optional warning-only P3; no coalescing.
- Preserve unrelated local changes; no Linear writes; report implementation limits honestly.

**Risks:**
- Opaque exact-repeat fallback may stop intentional repeated identical failures; explain user-visible stop and allow new turn, not silent replay.
- Broad argument-variant budgets deliberately deferred; different args can evade exact guard, bounded 128-key cap limits memory.
- Typed error classification propagation must be audited; text-only transient guessing avoided.
- Historical outbound schema still unavailable.