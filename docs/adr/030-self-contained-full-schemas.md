# ADR-030: Full-schema mode preserves caller definitions and rejects unusable references

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP17
- **Dependencies:** Accepted ADR-024 supplies validation. Accepted ADR-029 supplies effective serving/export identity.
- **Supersedes:** None
- **Superseded by:** None

## Context

The existing MCP render path intentionally returns placeholder schemas for some exposure states.
Provider adapters accept ToolSchema values, while SDK and tool definitions can use different reference structures.
dbunk needs full, nonrecursive, self-contained definitions without rewriting its final schema.
Where should schema eligibility and full rendering enforce that distinction?

## Decision drivers

- Serving and export must agree under the selected mode.
- Validate full draft 2020-12 offline.
- Preserve caller definitions and avoid guessed vendor support.
- Separate locally detectable failures from endpoint capability failures.

## Decision

We will add an independent opt-in full-schema mode to the shared effective catalog.
Validate the selected definitions before model requests, MCP full serving, or export.
Use ADR-024's validator rather than a second simplified implementation.

Selected schemas must be valid 2020-12, self-contained, and free of recursive reference cycles.
Resolve local definitions and embedded resources with correct URI/anchor semantics.
Never fetch missing resources from files or HTTP. A schema's dialect declaration is not permission to retrieve external data.
Detect invalid schemas, external references, and recursion locally as `schema_unusable`.

Owned tool generators may produce corrected self-contained definitions using the common documented tool-schema contract.
Prevent definition-name collisions when composing owned definitions.
Caller-owned worker and final-answer schemas are validated and offered unchanged, not flattened or rewritten to a vendor subset.
Do not attach provider strict flags.
A valid schema that an actual endpoint rejects produces `model_unsupported` through the selected probe, not a guessed startup blacklist.

This mode bypasses placeholder rendering for the selected effective tools.
It does not add tools, widen policy, or force the mode on ordinary users.
Existing terse/lazy behavior remains unchanged when it is disabled.
Path annotations remain metadata on owned arguments only, under ADR-029.

## Alternatives

- **Call a placeholder a full schema:** cannot support offline caller validation.
- **Inline or simplify every supplied schema:** changes caller meaning and violates preservation.
- **Resolve references on demand:** can read untrusted files or create network access.
- **Reject every unknown vendor keyword:** guesses support and confuses valid schema with endpoint capability.

## Consequences

Full definitions increase tool-schema size and validation cost.
Static reference analysis needs proper resource semantics, not a string-prefix check.
A schema may pass local validation and still fail the endpoint probe.
Owned schema changes require the contract-version review from ADR-029.

## Confirmation

Run future full-schema tests with contract, provisioning, and capability-probe tests.
Validate every owned selected schema without external retrieval.
Test local refs, anchors, duplicate definition names, invalid dialect/schema, external refs, and recursion.
Compare supplied JSON schema values before and after serving/provider conversion.
Test that only endpoint-detectable rejection becomes `model_unsupported`.
Rerun ordinary placeholder/lazy tests with the mode disabled.

## Anchors

_Anchors:_ src/tools.rs::render_list_tool, src/tools.rs::all_tools, src/tool_facade.rs::active_schemas, src/providers/mod.rs::ToolSchema

## Revisit when

The dialect, documented vendor baseline, or caller requirement for recursive schemas changes.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- The mode name and the maintained vendor-document evidence belong in the reviewed interface and test ledger before implementation.
