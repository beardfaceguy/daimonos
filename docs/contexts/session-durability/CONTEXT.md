# Session durability

Date: 2026-11-08

- **Canonical history:** The live AgentSession messages after a terminal turn commit.
- **Turn journal:** The authoritative in-memory suffix for a staged prompt or retry.
- **Recovery projection:** A persisted interruption-safe snapshot of completed messages and the accepted user prompt.
- **Checkpoint boundary:** An accepted prompt, completed provider response, or completed tool result. It excludes streamed-token fragments.
- **Owner lock:** The `flock` that the live writing process holds on `locks/<session-id>.lock`. Another process cannot claim the session while it is held. The OS releases it when the owner exits.
- **Writer epoch:** The store's ownership fence. An old runtime cannot overwrite a new owner's saves.
- **Capture generation:** The monotonically ordered payload identity that prevents a late write from replacing a newer snapshot.
- **Shutdown drain:** Waiting for the active-turn permit through its terminal persistence write, within the existing teardown deadline.

[ADR-014](../../adr/014-sqlite-session-memory.md) defines storage ownership.
[ADR-015](../../adr/015-session-protocol-v3.md) defines durability reporting.
[ADR-016](../../adr/016-cancelled-turn-partial-commit.md) defines interruption materialization.
[ADR-020](../../adr/020-durable-turn-checkpoints.md) records recovery checkpoints.
[ADR-033](../../adr/033-live-session-owner-lock.md) gives a live session one owning process.
[The investigation log](../../investigations/2026-10-28-acp-crash-history.md) records the reproduction and repair checks.

_Anchors:_ src/session_store.rs::SessionStore::claim_writer, src/agent.rs::TurnJournalWriter, src/session_core.rs::SessionCore::configure_turn_checkpoint, src/session_core.rs::TurnController::wait_idle, src/acp_cmd.rs::shutdown_session_handles_with
