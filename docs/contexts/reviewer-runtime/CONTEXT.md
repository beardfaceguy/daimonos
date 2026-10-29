# Reviewer runtime

Date: 2026-10-29

- **Run authority:** Immutable caller-selected limits on configuration, storage, access, and execution for one run.
- **State root:** Caller-owned location for internal artifacts. It is not permission to relocate explicit tool writes.
- **Effective catalog:** The definitions actually selected after registration, workspace checks, policy, tier, and schema mode.
- **Terminal call:** A designated final-answer call intercepted on the model side rather than executed as a tool.
- **Corrective retry:** A new model request after a failed ending. Ordinary evidence turns do not consume it.
- **Worker-only route:** The opt-in route that forwards non-final calls to caller-supplied MCP commands without local fallback.
- **Local audit:** Caller-requested run evidence. It is separate from optional analytics and OTLP export.
- **Logical input:** File, git-object, or index content read or scanned to serve a call. It is not physical I/O or output size.
- **Offline-tools authority:** A process network ceiling for local tool servers, distinct from the networked model side.
- **Tool-contract version:** Compatibility identity of tool definitions, separate from binary and source-commit identity.

The [package plan](../../handoffs/2026-10-28-dbunk-remaining-work.md#adr-inventory) indexes Accepted ADRs 021–032.
The user accepted those decisions and all four G0 interface groups on 2026-10-29 UTC.

[INVENTORIES.md](INVENTORIES.md) identifies state writers, discovery sources, network producers, and dispatch admissions.
[INTERFACES.md](INTERFACES.md) reserves the proposed public names, errors, counters, platform support, and producer/consumer seams.
Those review artifacts do not claim that implementation or runtime conformance is complete.

_Anchors:_ src/session.rs::Session, src/tool_facade.rs::active_schemas, src/providers/mod.rs::LlmProvider, src/managed_process.rs::ManagedChild
