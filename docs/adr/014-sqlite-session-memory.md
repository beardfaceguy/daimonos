# ADR-014: SQLite canonical session memory and versioned interchange

- **Status:** Accepted
- **Date:** 2026-08-29
- **Tracking:** Vikunja #1410; future tamper evidence #1411
- **Anchors:** `src/session_store.rs::SessionStore::write_record`,
  `src/session_interchange.rs::SessionCodec`

## Context

Per-session JSON replacement is crash-safe but a timed-out blocking writer may
finish after another runtime has loaded newer state. Neither ACP nor MCP defines
a portable session archive. OpenAI and Anthropic message objects are
provider-specific request formats rather than durable agent-session formats.

Future tamper evidence needs one stable mutation boundary and deterministic
payload bytes; it must not require every persistence caller to be rewritten.

## Decision

SQLite is the canonical session store. Each row contains the complete
provider-neutral session payload, indexed listing metadata, and a monotonically
increasing generation. A transaction accepts a write only when its generation
is greater than the stored generation. SQL remains private to `SessionStore`.
Schema bootstrap and every future migration run in an immediate transaction.
Version 1 has no predecessor to migrate; future schema bumps must add an
explicit forward migration before increasing the supported version.

Portable interchange is separate from storage. A format-neutral codec boundary
owns import and export; JSON is the first codec and uses the versioned
`daimonos.session` envelope. Import is explicit, requires a session id, and
atomically rejects duplicates. There is no automatic file migration.

Deletion removes payload state. The future tamper-evident layer may append
operation and payload digests plus deletion tombstones at the same transaction
boundary, then sign externally anchored checkpoints. It will not require
changing callers or retaining deleted private payloads.

## Consequences

- Late stale writers cannot replace a newer committed generation.
- Listing no longer walks one payload file per session.
- SQLite locking is bounded by configuration and filesystem operations remain
  off asynchronous executor threads.
- The archive is Daimonos-specific until a suitable vendor-neutral standard
  emerges; its explicit format and version permit compatible evolution.
- Cross-process authenticity is not provided yet. Task #1411 adds hash-chain
  verification, signed checkpoints, key rotation, and a threat model.
- The compatibility chat store is single-writer per session id. Daemon and ACP
  persistence use explicit capture generations.
