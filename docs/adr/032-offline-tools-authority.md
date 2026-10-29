# ADR-032: Offline-tools is a process-wide denial of network capability

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP20
- **Dependencies:** Accepted ADR-006 and ADR-017. Accepted ADR-021 and ADR-023 supply startup authority and eligibility.
- **Supersedes:** None
- **Superseded by:** None

## Context

A read-only workspace does not stop model discovery, OAuth, telemetry, KGL network access, or remote-service tools.
Disabling analytics upload alone can still collect and store analytics locally.
dbunk requires local evidence tools and Daimonos internal operations to make no network connection attempt.
Where must offline authority take effect before background services can create traffic?

## Decision drivers

- Observe no attempts, not merely unreachable endpoints.
- Disable collection as well as remote analytics delivery.
- Preserve local audit when explicitly requested.
- Keep credentialed model work separate from offline tool workers.

## Decision

We will resolve an opt-in offline-tools authority before network clients, discovery, or background exporters start.
It is a process-wide network ceiling that every run/session in that process must obey.
Do not create mixed sessions that re-enable networking under an offline process.
Do not mutate global environment as a substitute for explicit capability decisions.

Admit only Daimonos-owned local tools whose operations can satisfy that ceiling.
Remote-purpose tools are unavailable. An explicitly requested incompatible tool fails selection with a clear reason, not silent removal.
Disable internal update/version checks, remote discovery, OAuth network paths, telemetry, analytics collection, trace export, and networked substrate work.
Do not instantiate collection stores or background producers merely because upload is disabled.
Keep local process diagnostics and the separately requested ADR-026 audit available under their state policies.

A local git operation must retain ADR-017 no-fetch protections.
A local subprocess is not automatically offline merely because its tool name says read.
Reject unconstrained execution paths that cannot meet the guarantee.
Caller worker sandboxes remain caller-owned under ADR-028, not an implied guarantee over arbitrary peer programs.

Refuse a networked model/provider or probe in the same offline process at startup.
In split execution, the model side runs without offline-tools and the tool server runs with it.
Stdio and permitted local IPC do not grant outbound IP/DNS access.
When the option is absent, retain existing network and analytics behavior.
This profile is defense in depth, not a replacement for the caller's no-network namespace or container.

## Alternatives

- **Disable telemetry upload only:** leaves collection, discovery, and tool traffic.
- **Use an unreachable proxy/URL:** still makes connection attempts and may fall back.
- **Assume read-only implies offline:** filesystem permissions do not constrain networking.
- **Silently disable a selected model call:** hides an invalid process-role combination.

## Consequences

Startup must know the profile before constructing shared services.
Network and writer inventories become required regression fixtures.
A model-side reviewer and an offline tool server need separate processes, matching the PM-approved worker route.
Some local-looking features become unavailable if their network behavior cannot be constrained.

## Confirmation

Run future offline-tools tests in an attempt-observing namespace or equivalent syscall/DNS harness.
Invoke every admitted local read tool and exercise startup/shutdown/discovery paths.
Assert no outbound attempt, no analytics collection rows, and no exporter/discovery worker activation.
Test enabled local audit, forbidden remote tools, missing capabilities, and the proposed same-process model conflict.
Rerun normal analytics/observability behavior with the option disabled.

## Anchors

_Anchors:_ src/observability.rs::ObservabilityRuntime::initialize, src/analytics.rs::AnalyticsStore, src/agent_mcp.rs::parse_servers_json, src/provisioning.rs::build_tool_services, src/plugins/git_read/runner.rs::ReadRepo

## Revisit when

A local model transport, new network-capable internal subsystem, or explicitly brokered offline exception becomes a caller requirement.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Inventory every network producer before implementation. Profile and reason-code spellings remain proposed.
