# ADR-024: Validated terminal calls are model-side run endings

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP05
- **Dependencies:** Accepted ADR-001 and ADR-016. Accepted ADR-023 supplies explicit-name enforcement when enabled.
- **Supersedes:** None
- **Superseded by:** None

## Context

The core loop normally executes tool calls and returns assistant content.
dbunk needs one schema-valid JSON document without forcing provider tool choice.
Ordinary evidence-tool turns must continue without consuming corrective retries.
How should one terminal call become output while preserving tool pairing and bounded execution?

## Decision drivers

- Validate full draft 2020-12, including asserted date-time formats.
- Preserve caller schemas and provider-neutral loop ownership.
- Distinguish evidence turns, failed endings, and model-request limits.
- Never execute or forward the final-answer tool.

## Decision

We will intercept the caller-designated final-answer name on the model side before tool dispatch.
Compile the supplied schema once per run and offer it unchanged as that tool's input schema.
Success requires exactly one call to that name with valid arguments. Accompanying text or reasoning is allowed but not stdout output.
Emit only the validated JSON document and one newline on stdout.

A response with only ordinary tool calls is a normal evidence step.
A response with no calls, any final call, or an endpoint failure is an ending requiring success or typed failure handling.
For a failed mixed ending, execute none of its requested calls and pair every call with an explanatory error result.
Append validation errors and the corrective instruction from prompts, not Rust model-facing literals.

Maintain separate ending-attempt, corrective-retry, and model-request counters.
Ordinary tool steps consume model requests, not corrective retries.
The caller's model-request ceiling also covers probe requests and actual transport retry requests. No hidden request sits outside that ceiling.
A valid final answer on the last permitted request succeeds. Otherwise the ceiling yields `turn_limit`.
Preserve the requested structured-failure reasons and last validation errors, including `retries_exhausted`.
Numeric exit codes and public option names remain proposed until interface review.

Use the human-approved validator dependency: propose `jsonschema` exactly 0.60.2 with default features disabled.
Use draft202012 options with format assertion enabled. Do not enable file, HTTP, or async external resolvers.
Only resources embedded in the supplied document may resolve. Refuse external retrieval before model calls or external I/O.
Reuse this validator for schema eligibility. Do not build a simplified handwritten validator.
The dependency is proposed here, not added to Cargo.
Legacy agent output remains unchanged when disabled.

## Alternatives

- **Force tool choice or provider strict mode:** excluded by the caller's wire contract.
- **Parse free text after completion:** cannot guarantee one validated document.
- **Dispatch the final tool to a worker:** lets evidence-side code determine the run's output.
- **One retry counter for every tool turn:** rejects ordinary evidence gathering and fails to bound runaway loops correctly.

## Consequences

The loop gains an explicit terminal state and bounded counters.
Validator compilation and validation add CPU and memory cost.
Provider adapters keep wire parsing, while the core owns terminal meaning and result pairing.
Full validation cannot prove that a supported endpoint accepts every valid schema. The probe handles that distinction.

## Confirmation

Run future `tests/test_structured_output.py` with both mock wires and strict unanswered-call checks.
Test all requested keywords, local refs, forbidden external retrieval, and asserted invalid date-time values.
Test ordinary tool turns, mixed endings, zero retries, exhausted retries, endpoint failure, and exact stdout bytes.
Test probe/transport accounting and success on the last allowed model request.

## Anchors

_Anchors:_ src/agent.rs::AgentConfig, src/agent.rs::run, src/providers/mod.rs::LlmProvider, src/providers/mod.rs::ToolSchema, src/providers/mod.rs::validate_response_shape

## Revisit when

The schema dialect, validator security posture, or caller-defined terminal protocol changes.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Dependency evidence: [0.60.2 docs](https://docs.rs/jsonschema/0.60.2/jsonschema/) and [crate metadata](https://crates.io/api/v1/crates/jsonschema/0.60.2), checked 2026-10-28. Minimum Rust is 1.85.0.
