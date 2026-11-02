# ADR-023: Explicit tool policy applies at every dispatch boundary

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP06
- **Dependencies:** Accepted ADR-022. Accepted ADR-010 supplies the non-widening invariant.
- **Supersedes:** None
- **Superseded by:** None

## Context

The existing agent policy gates names, but an empty legacy list means unrestricted access.
MCP, batch, and Starlark use dispatch paths that do not all apply that gate internally.
dbunk needs an explicit allowlist that narrows existing restrictions and cannot be bypassed through delegation.
Where should selection and execution enforce this new authority?

## Decision drivers

- Explicit empty and absent lists must remain distinct.
- A listed composite must not grant unlisted inner tools.
- Served, discovered, probed, and executable names must agree.
- Existing host restrictions cannot be widened.

## Decision

We will represent an explicit run allowlist separately from the legacy policy.
The effective set is its intersection with existing restrictions and selected capability constraints.
An absent explicit list preserves current behavior. An explicitly empty list grants no tools.

Build the effective catalog from actual native, plugin, and caller-worker registrations.
Reject unknown requested names and an omitted configured final-answer name at startup.
Never add the final-answer name automatically or silently drop an invalid selection.
Report the resulting intersection.

Enforce the same immutable policy before every direct, batch, Starlark, generic tool, plugin, and remote dispatch.
Nested calls cannot reset or widen it. Apply it to lazy listings and schema lookup as well as execution.
A listed script may call only individually allowed inner names.
Model-side final-answer interception is an output operation, but its offered name must still be explicitly allowed.

The common policy service owns decisions. Frontend hooks are not the security boundary.
Refusals are typed and reach the local audit when enabled.
This decision does not silently claim that legacy MCP agent-env policy is now enforced when the explicit option is absent.

## Alternatives

- **Filter only model listings:** fabricated tool calls still execute.
- **Check only outer batch/script names:** delegation bypasses the list.
- **Union caller and environment lists:** restores permissions the caller removed.
- **Reuse empty legacy lists directly:** cannot express an explicit deny-all selection.

## Consequences

Every dispatch adapter must carry the run policy.
Caller workers need a registration-to-route inventory before startup validation completes.
Historic opt-out behavior tests remain separate from new secure-mode tests.
Refusals identify names without exposing denied payload contents.

## Confirmation

Run future `tests/test_tool_allowlist.py` with existing batch and provisioning tests.
Test empty, absent, unknown, narrowed, and missing-final-name selections.
Test every inner Starlark binding and generic tool call, not only dedicated bindings.
Assert that direct, lazy, probe, dump, and dispatch sets match for equal inputs.
Audit each refusal without executing its side effects.

## Anchors

_Anchors:_ src/safety.rs::SafetyPolicy::gate, src/ops/mod.rs::dispatch_with_progress, src/script.rs::dispatch_tool_by_name, src/tool_runner.rs::ToolRegistry::run, src/tools.rs::all_tools

## Revisit when

A new delegation mechanism, tool registration source, or per-operation permission model appears.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Final public option/error spellings remain subject to the reviewed interface ledger.
