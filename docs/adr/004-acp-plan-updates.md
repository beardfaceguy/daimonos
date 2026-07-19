# ADR-004: Provider-neutral execution plans with ACP Plan updates

**Date:** 2026-07-19
**Status:** Accepted
**Tracks:** Vikunja #992

## Problem

ACP clients such as Zed render `SessionUpdate::Plan` as native task progress,
but Daimonos had no agent-side plan concept and never emitted that update.
Inferring plans from prose would be unreliable, provider-specific, and unable
to represent explicit status transitions.

## Decision

### Agent-only `update_plan` tool

The built-in agent exposes `update_plan` with one argument: `entries`, the
complete ordered replacement plan. Every entry requires:

- `content`: non-empty human-readable task text
- `priority`: `high`, `medium`, or `low`
- `status`: `pending`, `in_progress`, or `completed`

An empty list clears the plan. Each update replaces the entire plan, matching
ACP v1 semantics. The tool is `AgentOnly`: available to Daimonos
agent/chat/ACP runtimes but omitted when Daimonos serves tools via MCP, where
the host agent owns its planning UI.

### Provider-neutral hook

`agent.rs` owns normalized plan types and an optional `PlanHook`. Valid tool
calls invoke the hook and produce a compact successful tool result; invalid
calls return an error result. Agent/chat can use the tool without a
presentation hook. ACP maps normalized entries to protocol `PlanEntry` values
and emits `SessionUpdate::Plan`.

### ACP presentation and replay

ACP suppresses generic tool-call chrome for `update_plan`; Zed receives only
its native Plan update. Tool calls/results remain in provider history so model
state and persisted sessions are self-contained. On `session/load`, valid
historical plan calls are parsed and replayed as Plan updates while their
generic tool-call/result presentation is suppressed.

### Prompt behavior

The embedded agent system prompt directs models to use plans only for
meaningful multi-step work, send the complete list on every change, and keep
statuses current. Simple tasks should not create a plan.

## Rejected alternatives

- **Infer plans from assistant prose:** ambiguous and cannot reliably track
  statuses.
- **ACP-specific planning in the core loop:** couples agent execution to one
  frontend and leaves chat/other providers inconsistent.
- **Expose `update_plan` from MCP-server mode:** the MCP host should own its
  plan protocol/UI; a remote planning side effect has no portable MCP meaning.
- **Separate mutable plan persistence:** duplicates information already
  represented in tool-call history and complicates compaction/session replay.

## Verification

- Pure parsing/normalization and invalid-input tests.
- Core loop test: hook invocation plus successful history result.
- Registry tests: included in agent schemas and excluded from MCP schemas.
- ACP protocol test: live Plan emission, no generic tool chrome, and
  `session/load` Plan replay.
