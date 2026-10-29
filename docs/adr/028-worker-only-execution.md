# ADR-028: Worker-only execution isolates model credentials from tool dispatch

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP10
- **Dependencies:** Accepted ADR-001 and ADR-003. Accepted ADR-021 and ADR-023. ADR-024 supplies terminal interception when selected.
- **Supersedes:** None
- **Superseded by:** None

## Context

AgentConfig currently treats remote dispatch as a fallback to ordinary local tools.
The legacy bridge can fail open, which is appropriate for its existing editor integration but not dbunk's reviewer.
The reviewer needs credentials on the model side and all executable tools on caller-sandboxed workers.
How can that route fail closed without changing the legacy bridge?

## Decision drivers

- No actual tool executes in the credential-bearing model process.
- Worker configuration and child environments come from the caller.
- The final-answer call is output, not evidence-side execution.
- Worker failure cannot activate local fallback.

## Decision

We will add an explicit worker-only execution route, separate from the legacy remote fallback.
Use the existing MCP client and stdio protocol for caller-supplied worker commands, including additional compute servers.
Discover routes before effective-policy validation and any capability probe.
Preserve explicit registration identity in offered definitions and audit records.

Route every non-final call through that worker route or refuse it.
Do not try native opcode, plugin, script, or local fallback dispatch first.
When structured final output is enabled, intercept and validate its terminal name locally under ADR-024.
Never forward that call to a worker.

Start each worker from a cleared environment with only the documented caller-controlled variable map.
Do not inherit the model process environment, provider configuration, credential values, or credential-bearing initialization metadata.
Validate explicit argv/environment overrides against model-credential disclosure. Caller control does not authorize accidental credential forwarding.
Resolve executable lookup from caller-controlled settings, not inherited hidden PATH state.
Use only approved tool-call data and bounded results on the MCP channel.

Worker exit, stalled responses, discovery failure, or loss of a required route aborts the run with a distinct failure.
Stop further tool execution and reap owned processes through the existing managed lifecycle.
Do not reconnect transparently or complete remaining calls locally.
Caller workers remain responsible for their filesystem and network sandbox. Daimonos does not claim to sandbox arbitrary worker programs.
Keep the existing ACP bridge behavior unchanged when this option is absent.

## Alternatives

- **Remove credentials from local tool arguments only:** the same process still holds credential-bearing state.
- **Use legacy fallback directly:** worker failure can execute locally.
- **Inherit the normal process environment:** hidden credentials and ambient configuration can reach workers.
- **Implement a second worker protocol:** duplicates an existing client without a caller requirement.

## Consequences

An unavailable worker makes the opt-in route unusable by design.
Startup ordering must complete discovery before catalog freeze.
An explicit command/environment contract costs more setup but lets dbunk choose its own container or namespace.
The model side still has model network access. The tool side's isolation is independently testable.

## Confirmation

Run future worker-isolation and probe tests with a recording fake worker and a no-network worker container.
Inspect child env, argv, initialization, and calls for the exact model credential.
Test ordinary and compute calls, local refusal, terminal interception, worker death, timeout, and reaping.
Assert no local side effect or subsequent dispatch after failure.
Rerun existing ACP bridge tests with worker-only mode absent.

## Anchors

_Anchors:_ src/agent.rs::AgentConfig, src/agent.rs::run, src/agent_mcp.rs::parse_servers_json, src/mcp_bridge.rs::McpBridge, src/managed_process.rs::apply_environment, src/managed_process.rs::ManagedChild::terminate

## Revisit when

Workers need a non-stdio transport, credential delegation, or reconnection semantics that the caller explicitly accepts.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Public command/environment field names, route naming, and failure-code numbers need interface review.
