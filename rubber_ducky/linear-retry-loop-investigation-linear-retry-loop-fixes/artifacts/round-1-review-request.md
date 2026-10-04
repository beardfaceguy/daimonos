**Objective:** Prevent recurrence of repeated deterministic remote MCP errors while preserving legitimate searches, retries, write semantics and schema optionality.

**Context:** Evidence in ~/.daimonos/acp-sessions/sessions.sqlite3, ~/.daimonos/analytics.db and ~/.local/state/daimonos/logs/daimonos.20727; failed turn 2026-10-01 18:02:23–18:08:11 UTC. src/script.rs local dispatch, src/loop_detector.rs reset-on-novelty, src/mcp_bridge.rs typed schema/one-request dispatch, src/providers/openai.rs strict:false. Existing routing test and 14 detector unit tests pass. Read-only diagnosis complete; no implementation fixes yet.

**Steps:**
- P1: Clarify agent/MCP prompts that scripts only batch native/local tools; emit actionable unsupported-remote script errors without changing dispatch routing.
  - Acceptance: Rust fail-fast remote-name test and prompt-boundary tests; direct remote tools unaffected.
- P2: Specify and implement bounded per-turn repeated deterministic-error budget alongside novelty detector. Keep pressure across superficial argument changes and unrelated reads; use typed signals where available, not blanket is_error classification. Select concrete thresholds/reset policy before coding.
  - Acceptance: Rust red-green replay test of recovered 28-round trace reaches bounded stop.
  - Acceptance: Tests preserve transient recovery, intentional changed-query searches, successful progress, per-turn reset and bounded memory.
- P3: Assess duplicate same-batch read coalescing only with explicit trustworthy safety metadata; defer or use warning-only if unavailable. Never deduplicate unknown/mutating tools. Preserve results for all original IDs.
  - Acceptance: Safe duplicate read dispatches once with two paired results; writes twice; test error/cancellation correlation.
- P4: Reproduce schema serialization through mocked MCP and provider wire: optional customView with minLength 1 and absent required, additionalProperties false, nested schema and nullable union. Fix only demonstrated loss; no required-to-null adapter unless proven necessary.
  - Acceptance: End-to-end schema fidelity/omission regression tests without real Linear mutations.
  - Acceptance: Document historical outbound schema uncertainty; no speculative requiredness patch.
- P5: Add bounded opt-in safe schema hashes/snapshots, generation/tool correlation and explicit cancellation initiator/reason. Avoid credential, argument or full provider-body logging.
  - Acceptance: Retention/redaction tests and cancellation-source tests; disabled mode unchanged.

**Acceptance criteria:**
- Rust unit tests precede new Rust features; mock integration coverage where appropriate.
- Run fmt, clippy, focused and full available suites with explicit missing-dependency caveats.
- Ship independent reviewable commits; preserve unrelated working-tree changes; no production Linear mutations.

**Risks:**
- Deterministic errors lack consistent remote typed classification.
- Coalescing unsafe operations can change externally visible behavior.
- No historical wire-schema capture; current upstream evidence does not prove historical schema.
- Diagnostic retention and redaction must not leak sensitive data.