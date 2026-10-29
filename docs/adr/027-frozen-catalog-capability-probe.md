# ADR-027: A single capability probe validates the frozen run tool catalog

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP09
- **Dependencies:** Accepted ADR-023 and ADR-024. ADR-025 applies only when its independent transport profile is selected.
- **Supersedes:** None
- **Superseded by:** None

## Context

A model endpoint can reject tool schemas or return unsuitable tool behavior before a review starts.
Lazy exposure and worker discovery can otherwise change tool definitions between a test and real work.
The PM requires one unretried probe using exactly the final run definitions.
What startup boundary makes that probe meaningful without executing its requested tools?

## Decision drivers

- Discover the actual catalog before testing it.
- Require the caller's final-answer schema and valid probe payload.
- Permit text/reasoning beside one valid final call.
- Perform no review tools after a failed probe.

## Decision

We will resolve registrations, worker discovery, policies, and selected schema mode before freezing one model-facing catalog for the run.
Freeze names, descriptions, and schemas. Probe and subsequent model requests consume that same snapshot.
Lazy introspection cannot silently add or rewrite offered definitions after the freeze.
Worker changes or failures do not trigger an untested replacement catalog.

Probe remains an independent opt-in option that requires structured final output.
Reject probe-without-final-output at startup. Do not require local audit or controlled transport merely to enable the probe.
Use the selected run wire and transport. Regardless of other retry settings, this probe has one attempt and no redirect following.
When controlled transport is selected, its exact body/header rules apply.
Validate the caller's minimal probe payload locally before sending anything.
The instruction asks for the designated final tool and that payload.

Accept only one call to that final name with schema-valid arguments.
Text/reasoning alongside it is allowed. Do not demand byte-for-byte equality with the suggested payload.
Never execute any probe call, including an ordinary evidence or worker call.
Text-only, wrong/multiple calls, invalid arguments, timeout, 3xx, and endpoint rejection yield `model_unsupported` and no real work.
Record probe outcome in the local audit when enabled.
The request uses the shared model-request ceiling from ADR-024.

## Alternatives

- **Probe with a tiny substitute schema:** does not test the actual definitions.
- **Probe before worker discovery:** misses tools that the run will expose.
- **Retry until a valid answer arrives:** hides capability failure and violates the single-request requirement.
- **Execute a returned evidence call:** lets a failed preflight perform review work.

## Consequences

A probed run gives up mid-run catalog mutation.
Startup may launch explicitly supplied workers for discovery before model preflight, but never invokes their review tools.
A passing probe is a sampled capability check, not proof that later answers are correct.
Audit, transport, and structured endings remain separately selectable except for the required final-output dependency.

## Confirmation

Run future capability-probe and request-control tests on recording Chat and Messages endpoints.
Compare exact probe/run catalog snapshots and record actual request counts.
Test one valid final call with reasoning, every rejected outcome, and a final name omitted from the allowlist.
Assert zero tool invocations after every failed probe.
Test probe with and without the independent audit/transport options.

## Anchors

_Anchors:_ src/providers/mod.rs::LlmProvider, src/providers/mod.rs::ToolSchema, src/tool_facade.rs::active_schemas, src/tools.rs::render_list_tool, src/mcp_bridge.rs::McpBridge::tools

## Revisit when

The caller requests catalog mutation or a different preflight protocol during one run.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Exact startup reporting and exit-code numbers remain proposed. The single-attempt and final-output requirements are fixed by the PM.
