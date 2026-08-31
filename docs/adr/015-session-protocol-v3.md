# ADR-015: Protocol v3 ordered timeline and durability truth

- **Status:** Accepted
- **Date:** 2026-08-30
- **Tracking:** Vikunja #1338 and #1407
- **Anchors:** `src/session_timeline.rs::TimelineReducer`,
  `src/session_core.rs::SessionCore::publish_durability_status`

## Context

Protocol v2 split transcript and tool calls into independent arrays, so clients
could not reconstruct chronological interleaving. Its boolean truncation flag
also could not describe how much history was omitted. Persistence failures were
internal state only, leaving attached TUI and Android clients unable to warn
that visible conversation state was not durable.

Both fixes change the canonical snapshot and must share one protocol bump.
Publishing either independently would assign two incompatible schemas the same
version or force consecutive client lockouts.

## Decision

Protocol v3 replaces wire transcript/tool arrays with one ordered timeline.
Fold-minted occurrence ids are distinct from provider tool ids. Non-terminal
tool control state remains mandatory in `active_tools`, independent of
contiguous history trimming. `HistoryWindow` reports retained and omitted
history, and oversized content is explicitly marked after bounded fitting.
Daemon snapshots, reconstructed history, Rust frontends, and Android apply the
same timeline rules.

Snapshots also carry a privacy-safe `DurabilityStatus`, and class changes are
sequenced as `durability_status_changed` events:

- `saved` — every registered canonical mutation is committed;
- `unsaved` — visible canonical state has not yet entered its save attempt;
- `saving` — the serialized persistence owner is capturing or writing;
- `degraded` — bounded persistence attempts were exhausted;
- `superseded` — a newer writer epoch permanently invalidated this runtime.

Transient write errors that recover inside one retry pass do not publish
`degraded`. Conversation events remain usable while durability is non-saved;
clients render persistent warnings rather than terminating the session.
Frontend-local defaults are not authoritative and must not be rendered as
durability truth until the initial canonical snapshot has been applied.

## Consequences

- v2 clients are cleanly rejected; Rust, Android, and contract fixtures move to
  v3 together.
- Ordered timeline and durability transitions consume the same canonical event
  sequence, so snapshot recovery cannot reorder either truth.
- `saved` remains visually unobtrusive. TUI and Android continuously display
  non-saved states.
- Future persistence protocols may add diagnostic detail, but raw filesystem,
  SQLite, or user-content errors must never cross this wire boundary.
