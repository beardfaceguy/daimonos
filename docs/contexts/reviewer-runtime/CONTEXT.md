# Reviewer runtime

Date: 2026-11-03

- **Run authority:** Immutable caller-selected limits on configuration, storage, access, and execution for one run.
- **State root:** Caller-owned location for internal artifacts. It is not permission to relocate explicit tool writes.
- **Read-only workspace:** The `--workspace-read-only` run state. Every Daimonos-initiated workspace mutation is refused (`workspace_write_forbidden`) and opaque, unmediatable tools are refused (`tool_policy_unsupported`) before any effect begins at every dispatch boundary and in the shared agent run loop (`daimonos agent`/`chat`/`acp`), so `execute_script` is refused before any Starlark runs in every frontend; internal state uses the state root (the startup-guarded internal writers are the single registry `run_policy::INTERNAL_WRITERS`, cross-checked against INVENTORIES.md). Admission table: `src/run_policy.rs` (`ToolAdmission`, `EffectClass`). Implemented (WP04).
- **Confinement root:** Fixed run-scoped boundary for authorized filesystem access. `set_cwd` does not change it.
- **Working directory:** Mutable session base for relative paths. With confinement enabled, the filesystem authority enforces the confinement root for every operation.
- **Effective catalog:** The definitions actually selected after registration, workspace checks, policy, tier, and schema mode.
- **Terminal call:** A designated final-answer call intercepted on the model side rather than executed as a tool.
- **Corrective retry:** A new model request after a failed ending. Ordinary evidence turns do not consume it.
- **Worker-only route:** The opt-in route that forwards non-final calls to caller-supplied MCP commands without local fallback.
- **Local audit:** Caller-requested run evidence (`--audit-path`). A private per-run JSONL file (`O_CREAT | O_EXCL`, mode `0600`, no append) with a versioned `run_start`…`run_end` lifecycle plus `model_request`/`tool_call`/`tool_refusal` records sharing one `run_id` and monotonic `sequence`. Separate from optional analytics and OTLP export; works without controlled transport or a model profile. ADR-006 D6 redaction (credential variable name not value, base-URL host only; argument/result SHA-256 of canonical bytes, never contents). Fail-closed per ADR-026: a required-write failure stops the run (exit 23 `audit_write_failed`), unlike OTLP's fail-open. Module: `src/run_audit.rs` (`RunAudit`, `AuditFailure`). Implemented (WP08). The typed `AuditEvent` (reserved name) renders the stream JSON. Written only by `agent --print`; other frontends refuse `--audit-path` at startup (`audit_unsupported_frontend`, exit 2) so no run is falsely audited (WP08, r2).
- **Logical input:** File, git-object, or index content read or scanned to serve a call. It is not physical I/O or output size.
- **Offline-tools authority:** A process network ceiling for local tool servers, distinct from the networked model side.
- **Tool-contract version:** Compatibility identity of tool definitions, separate from binary and source-commit identity.
- **Final output:** The opt-in `daimonos agent --final-output` mode whose only success is exactly one validated UTF-8 JSON document and a trailing newline on stdout; otherwise it ends with a distinct typed failure.
- **turn_limit:** The final-output reason emitted when the caller's `max_model_requests` ceiling refuses a further model request; the run exits 24 (`model_request_limit`) and sends nothing.
- **Typed failure object:** The bounded final-output failure JSON (`reason`, `validation_errors`, `attempts`, `model_requests`) written to stderr or an authorized state destination, never a free-text message.

The [package plan](../../handoffs/2026-10-28-dbunk-remaining-work.md#adr-inventory) indexes Accepted ADRs 021–032.
The user accepted those decisions and all four G0 interface groups on 2026-10-29 UTC.

[INVENTORIES.md](INVENTORIES.md) identifies state writers, discovery sources, network producers, and dispatch admissions.
[INTERFACES.md](INTERFACES.md) reserves the proposed public names, errors, counters, platform support, and producer/consumer seams.
Those review artifacts do not claim that implementation or runtime conformance is complete.

_Anchors:_ src/session.rs::Session, src/tool_facade.rs::active_schemas, src/providers/mod.rs::LlmProvider, src/managed_process.rs::ManagedChild
