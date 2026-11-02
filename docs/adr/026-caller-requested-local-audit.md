# ADR-026: Caller-requested audit is local and separate from optional telemetry

- **Status:** Accepted
- **Date:** 2026-10-28
- **Deciders:** Repository maintainer, by explicit user approval on 2026-10-29 UTC.
- **Consulted:** None recorded.
- **Informed:** None recorded.
- **Spec:** Local dbunk requests and PM amendments, sealed in the [package plan](../handoffs/2026-10-28-dbunk-remaining-work.md).
- **Packages:** WP08
- **Dependencies:** Accepted ADR-006. Accepted ADR-021 applies when an explicit state root is selected.
- **Supersedes:** None
- **Superseded by:** None

## Context

Daimonos already has SQLite analytics, local logs, and optional fail-open OTLP observability.
Those mechanisms do not supply dbunk's caller-owned JSONL run record.
An audit that silently stops writing cannot prove which policy, requests, and tools ran.
Who owns the new record, its privacy boundary, and its failure semantics?

## Decision drivers

- Audit must work without controlled transport or structured endings.
- Credentials and source contents must never enter the record.
- Startup, refusal, error, and final status need the same recording path.
- Requested audit failure must not be mistaken for a successful audited run.

## Decision

We will add an independent opt-in local audit sink owned by the run lifecycle.
Use shared core and dispatch events, with provider adapters supplying wire-derived metadata.
Do not replace analytics, rotating logs, or OTLP.

The caller chooses the destination outside evidence. With an explicit state root, it must also stay inside that root.
Validate/open the destination before work. Keep one serialized JSONL record stream for the run.
Record the five configuration fields with only the base URL host, reported model identity, probe status, effective allowlist/root, requests, tool decisions, and final status.
Unavailable metadata is explicit, not invented.

For tools, record name, argument/result SHA-256, duration, and allowed/refused state.
Hash the bytes observed at the dispatch boundary. Do not retain contents in audit events.
Never record credential values, full file contents, raw headers, provider error bodies, or URL userinfo/path/query.
Redact before event construction, not only during file writing.
State paths and effective roots in this caller-owned local audit are not permission to export them through OTLP.

Use fail-closed semantics when audit is explicitly enabled.
An open/write/flush failure aborts the run before further model/tool work and cannot produce successful final output.
Write the terminal audit record before publishing success. Error diagnostics must not echo secrets or require the failed sink to work.
This is distinct from ADR-006's optional fail-open export policy.
Audit remains off by default. No new remote exporter is introduced.
A flushed stream is not a claim of physical power-loss durability.

## Alternatives

- **Reuse OTLP as the required record:** requires network service and inherits different failure/privacy semantics.
- **Reuse analytics rows:** lacks complete request/refusal/final run evidence.
- **Log raw payloads then redact:** secrets already entered telemetry memory or disk.
- **Continue successfully after a required write fails:** falsely claims a complete audited run.

## Consequences

Audit writes add local I/O and can become an intentional run failure.
A serialized sink prevents interleaved partial lines from concurrent calls.
Enabled/disabled behavior must remain independent of transport and final-output options.
The proposed required-audit semantics need explicit maintainer review.

## Confirmation

Run future `tests/test_run_audit.py` with startup failure, refusal, worker death, limit, and success cases.
Simulate open, mid-run write, and final flush failures. Assert no successful output or later work.
Scan every event and diagnostic for exact secret values, URL credentials, and synthetic file markers.
Enable audit without controlled transport or structured output. Verify OTLP remains a separate optional system.

## Anchors

_Anchors:_ src/observability.rs::ObservabilityRuntime::initialize, src/analytics.rs::AnalyticsStore, src/agent.rs::AgentConfig, src/script.rs::dispatch_tool_by_name

## Revisit when

Audit rotation, append/resume semantics, stronger durability, or external audit delivery becomes a caller requirement.

## Open questions

- The user accepted this decision on 2026-10-29 UTC. Remaining interface questions do not imply implementation approval.
- Final event field names, bounds, and numeric exit codes need interface review before implementation.
