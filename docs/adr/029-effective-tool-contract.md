# ADR-029: One effective catalog supplies serving, export, and contract identity

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP15, WP16
- **Dependencies:** Accepted ADR-023. ADR-028 supplies worker discovery when caller workers are selected.
- **Supersedes:** None
- **Superseded by:** None

## Context

Tool definitions come from native declarations, available plugins, policies, workspace checks, and exposure tiers.
The MCP rendering path can deliberately strip schemas or change descriptions.
A static handwritten dump would not describe what the server actually offers.
Which catalog should the caller version, inspect, and use for path metadata?

## Decision drivers

- Equal inputs must produce equal served and exported definitions.
- Binary/source identity and tool-contract identity are separate.
- Owned path metadata must be complete without rewriting caller schemas.
- Contract inspection must not start a model run or live session store.

## Decision

We will resolve one effective catalog and use it for MCP serving and deterministic JSON export.
Inputs include workspace, explicit configuration, capability/allowlist policy, plugin availability, tier, and schema mode.
Do not maintain a separate export-only registry or derive the contract from tool names alone.
Sort output deterministically while preserving each selected schema's meaning.
The exported tool-definition array must equal the served array for those same inputs.

Give the tool contract an independent semantic version and a changelog.
Breaking definition changes increment its major version. Additive compatible changes increment minor; corrections use patch.
Expose that version through dedicated MCP initialization metadata and export metadata.
Do not overload binary-version output or the container's source-commit identity.
The metadata field location/name remains proposed until SDK interface review.

Path annotations apply only to Daimonos-owned filesystem-path, glob, and walk arguments, including array elements.
Do not annotate opaque script/command strings as paths.
Do not rewrite or require annotations on caller-owned workers or final-answer schemas.
Annotations document semantics. ADR-022 performs enforcement.

Export does not start an agent/model run, session database, analytics writer, or implicit remote discovery.
If the caller explicitly selects worker contributions, resolve their definitions through the same explicit discovery path and close owned workers afterward.
Document that explicit worker discovery can start those commands. It is not a promise of side-effect-free arbitrary worker introspection.
Ordinary serving defaults remain unchanged.

## Alternatives

- **Dump the complete built-in catalog:** cannot represent the effective server list.
- **Version only the binary:** hides contract compatibility from callers.
- **Export stripped placeholders as full definitions:** misstates the selected mode.
- **Infer path fields by spelling:** misses arrays and falsely labels opaque code.

## Consequences

Serving and export share resolution code and fixtures.
Dynamic plugin/worker availability remains an explicit input, not nondeterminism to hide.
A separate contract version requires deliberate changelog maintenance.
Source identity in local images remains a packaging check, not a second tool version.

## Confirmation

Run future tool-contract and path-annotation tests with existing handshake/provisioning tests.
Compare dump and tools/list under matching workspace/configuration/policy/tier/schema inputs.
Check stable ordering, standalone export, distinct version metadata, and breaking-change version rules.
Review positive/negative owned path inventories. Caller schemas remain byte-semantically unchanged.

## Anchors

_Anchors:_ src/tools.rs::all_tools, src/tools.rs::render_list_tool, src/tool_facade.rs::active_schemas, src/provisioning.rs::build_tool_services, src/mcp_bridge.rs::McpBridge::tools

## Revisit when

New dynamic registration sources or a public contract migration require a different compatibility policy.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- The initial contract version and exact metadata/export field names need interface review.
