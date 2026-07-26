# ADR-009: Native agent-to-agent coordination (agent mail)

**Date:** 2026-07-24
**Status:** Accepted
**Tracks:** Vikunja project 183, task #1057 (feature), #1053 (evaluation + design lessons)
**Relates to:** ADR-003 (ACP-MCP bridge), ADR-006 (LLM observability),
ADR-008 (programmatic LLM sub-calls, #1050 reachability)

## Context

Daimonos harness agents increasingly run as a *fleet*: several independent
daimonos processes (each ACP session, one-shot `agent` run, or `chat` REPL)
operate on the same target workspace at the same time. Today they cannot see or
talk to one another. Two agents editing the same repo have no shared identity,
no way to leave each other a message, and no way to signal "I'm working on
these files" — so they duplicate work, clobber edits, and re-derive the same
context.

Phase 0 evaluated an external tool, **MCP Agent Mail**
(`Dicklesworthstone/mcp_agent_mail_rust`), as a model. It is a strong
conceptual fit — persistent agent identity, directed message passing (subjects,
to/cc, threads, acks, importance; deliberately **no broadcast**), and advisory
file reservations — but it ships a reproducible **stack-overflow crash** in its
git-archive reconstruction path and depends on an HTTP/loopback transport with
its own auth ergonomics. Those findings, captured in #1053, are the *hard
constraints* below, not suggestions.

This ADR decides how daimonos builds the equivalent capability **natively**,
reusing its own SQLite prior art, and — above all — **where the shared store
lives and how separate daimonos processes share it**, which is the pivotal,
hard-to-reverse decision.

### What this is (and is not)

This is a **messaging system between agents**, with identity as its foundation
and advisory file reservations as a lighter third feature. It is *not* a file
lock manager. Reservations are advisory rows ("BlueLake intends to edit
`src/api/*.rs` until T"), surfaced *through* the messaging layer; they never
take an OS lock and never block a write. The coordination value is the mail.

## Hard constraints (from #1053 — the "what not to do" lessons)

1. **No unbounded recursion** in any store / replay / traversal path. Use
   explicit, bounded iteration (SQL + capped loops), a hard depth cap, and
   cycle detection. A stack overflow is an abort, not a catchable error — one
   adversarial input takes the whole process down. This is the single most
   important thing to get right.
2. **A coordination fault must not crash the agent.** The store's read/write
   paths must be `Result`-typed and panic-free, and the tool layer must be
   **fail-open**: if coordination is unavailable, the tool returns a soft error
   and the agent continues its turn.
3. **Single source of truth.** One SQLite database is *the* authority. Do **not**
   dual-source (live DB + a git archive with reconstruction-on-read) — that is
   the exact class of bug that crashes agent_mail. Any audit/export is
   **write-only**, never read back.
4. **Small tool surface.** Identity + inbox + file-reservations only. No product
   bus, build slots, TUI, or semantic search in the MVP.
5. **No HTTP/auth.** A native in-daemon/in-process store avoids the
   loopback-and-bearer-token complexity entirely.

## Decision

### D1 — One dedicated coordination SQLite DB per workspace, opened directly by every process (no broker)

Coordination state lives in a **single, dedicated SQLite database**, one per
workspace, that **every daimonos process opens directly** in WAL mode. This is
options **(a) direct-open + (c) dedicated store** from #1057, combined; it is
explicitly **not** option (b) (routing through a broker daemon).

- **Location:** `~/.daimonos/coordination/<workspace-key>.db`, where
  `<workspace-key>` is a stable hash of the *canonicalized* workspace path.
  This follows the existing daimonos convention that global state lives under
  `~/.daimonos/` (e.g. `~/.daimonos/analytics.db`), keeping the coordination DB
  **out of the target repo tree** — daimonos is an installed binary, not a repo
  artifact. The canonical workspace path is the "project key" (mirroring
  agent_mail's project key and KGL's per-workspace scoping), and it is the
  **trust boundary**: agents operating on the same workspace can coordinate;
  others never see the DB.

- **Sharing mechanism:** processes coordinate *only* by reading and writing rows
  in that one shared file — process A does `INSERT INTO message …`, process B
  does `SELECT … WHERE recipient = …`. No IPC, no shared memory, no broker, no
  socket. This is the identical pattern `src/kgl/store.rs` and `src/analytics.rs`
  already use safely across concurrent daimonos processes.

  ```
  daimonos proc #1 (agent "BlueLake")   ─┐
  daimonos proc #2 (agent "GreenCastle")─┼─► open(coordination/<ws>.db)  [SQLite WAL]
  daimonos proc #3 (chat / one-shot)    ─┘
  ```

- **Concurrency:** `PRAGMA journal_mode=WAL` + `PRAGMA synchronous=NORMAL` +
  a `busy_timeout` (from `[coordination] busy_timeout_ms`, default 5000, matching
  `[kgl]`). WAL gives one-writer/many-readers without blocking; a contended
  writer waits out the busy timeout instead of surfacing `SQLITE_BUSY`.

- **No interaction with other daimonos DBs.** The coordination DB does **not**
  read from or write to KGL (`.kgl/kgl.db`), analytics (`~/.daimonos/analytics.db`),
  or the JSON session store. It is a single source of truth for coordination
  state *only*, with **zero cross-DB reads** — which is also what keeps us clear
  of the lesson-3 reconstruction-on-read crash class. A dedicated file also lets
  coordination carry its own schema, migrations, and PRAGMA tuning without
  risking the other stores.

**Why not (b), a broker daemon:** the existing Unix-socket daemon (`main.rs`) is
*per-connection* and is **not guaranteed to be running** for one-shot/chat
invocations. Making it the store owner would force a new always-on broker and
turn a single coordination bug into a fleet-wide fault — the opposite of
constraint 2. WAL already gives safe multi-process access for free.

### D2 — Identity: stable, memorable, per-workspace names

An agent registers a name in the coordination DB, scoped to the workspace.

- Names are **memorable adjective+noun** handles (e.g. `BlueLake`,
  `GreenCastle`), unique per workspace. If a caller does not supply one, the
  store mints a random available name.
- Registration is **idempotent**: re-registering an existing name updates its
  `last_seen` and metadata (program/model/task description) rather than
  erroring. This is how a name survives across turns and process restarts within
  a session.
- Each agent row records the daimonos **session id** (the external
  agent-runtime session id when present, per ADR-006 / `external_session_id`) and
  the canonical workspace, so a name maps back to a concrete session.
- Identity is the foundation the other two features build on: messages and
  reservations reference agents by name.

### D3 — Messaging: directed mail, threads, acks, importance — no broadcast

The heart of the system. Messages are inserted as rows and delivered by
selection into a recipient's inbox.

- `send_message`: `to` + optional `cc` (arrays of agent names), `subject`,
  `body`, `importance` (`low`/`normal`/`high`/`urgent`), `ack_required` (bool),
  optional `thread_id` / `reply_to`. Delivery is **directed**: at least one
  recipient is required. **Broadcast is deliberately unsupported** — it is the
  primary spam vector, and agent_mail's deliberate omission; we keep the
  omission.
- `fetch_inbox`: returns an agent's messages, newest-first, with filters
  (`unread_only`, `importance`, `since`, `limit`). Reading marks delivered rows
  read (or `mark_read` does so explicitly).
- `mark_read` / `acknowledge`: per-recipient read and ack receipts (an ack is a
  lightweight non-textual reply for `ack_required` messages).
- **Threads:** a reply carries the parent's `thread_id` (or, if the parent had
  none, the parent's id becomes the thread id). Thread reconstruction is a
  **bounded** operation: a flat `SELECT … WHERE thread_id = ? ORDER BY created`
  with a hard row cap — **never a recursive walk** (constraint 1). Reply chains
  cannot cause the reconstruction-on-read recursion that crashes agent_mail.

### D4 — Advisory file reservations

The lighter third feature — a soft "I'm working here" signal, not a lock.

- `reserve_paths`: claim one or more glob patterns with a TTL and optional
  `exclusive` flag and reason. Returns granted reservations plus any conflicts
  with *other* agents' active exclusive reservations.
- `renew` / `release`: extend or drop reservations held by the caller.
- `check_conflicts`: read-only pre-edit check of paths against active
  reservations, ignoring the caller's own.
- Matching is **symmetric glob** with a **bounded** candidate set (no recursive
  directory walk; the store compares patterns, not the filesystem tree).
  Expired reservations (TTL elapsed) are inert and lazily pruned.
- Reservations are **advisory only** — they never block a write. A pre-commit
  guard that *enforces* them is explicitly out of MVP scope (a possible later
  ticket).

### D5 — Tool surface: native opcode-facade tools, reachable from the agent loop

The coordination tools are exposed as **native opcode-facade tools**, not
execute_script builtins, for the MVP. They flow through the established path:
`tools::build_request` → `ops::dispatch` → a new coordination `ops` module →
`Response`, and are dispatched in-loop via `tool_facade::invoke` — the **same
reachability path that gated execute_script** (#1050). This guarantees the
internal agent loop (chat/one-shot/ACP) can call them, not just external MCP
clients.

Because the compact `Op` struct (`c`/`p`/`s`/`n`/…) is too narrow to carry rich
mail payloads cleanly, coordination uses a **dedicated opcode carrying a JSON
body** (a `verb` + params object), decoded by the coordination ops module. This
keeps the wire protocol stable and avoids overloading positional fields.

MVP tool set (small, per constraint 4):

| Feature | Tools |
|---|---|
| Identity | `register_agent`, `list_agents` |
| Messaging | `send_message`, `fetch_inbox`, `mark_read`, `acknowledge`, `reply_message` |
| Reservations | `reserve_paths`, `renew_reservations`, `release_reservations`, `check_conflicts` |

### D6 — Schema (initial)

A dedicated schema with a `schema_version` row (like `SESSION_PERSIST_VERSION`),
so a future format change is detected, not mis-parsed.

```sql
CREATE TABLE agent (
  id            INTEGER PRIMARY KEY,
  name          TEXT NOT NULL UNIQUE,        -- adjective+noun, unique per workspace DB
  workspace     TEXT NOT NULL,               -- canonical path (redundant; one DB per ws)
  session_id    TEXT,                        -- daimonos/agent-runtime session id
  program       TEXT, model TEXT, task TEXT, -- optional profile metadata
  inception_ts  TEXT NOT NULL,
  last_seen_ts  TEXT NOT NULL
);

CREATE TABLE message (
  id          INTEGER PRIMARY KEY,
  thread_id   INTEGER,                       -- NULL for a new thread; else the root id
  reply_to    INTEGER,                       -- parent message id, or NULL
  sender      TEXT NOT NULL,                 -- agent.name
  subject     TEXT NOT NULL,
  body        TEXT NOT NULL,
  importance  TEXT NOT NULL DEFAULT 'normal',
  ack_required INTEGER NOT NULL DEFAULT 0,
  created_ts  TEXT NOT NULL
);
CREATE INDEX message_thread ON message(thread_id, created_ts);

CREATE TABLE recipient (
  message_id  INTEGER NOT NULL,
  agent_name  TEXT NOT NULL,
  kind        TEXT NOT NULL,                 -- 'to' | 'cc'
  read_ts     TEXT,                          -- NULL until read
  ack_ts      TEXT,                          -- NULL until acknowledged
  PRIMARY KEY (message_id, agent_name)
);
CREATE INDEX recipient_inbox ON recipient(agent_name, read_ts);

CREATE TABLE reservation (
  id           INTEGER PRIMARY KEY,
  agent_name   TEXT NOT NULL,
  pattern      TEXT NOT NULL,                -- glob
  exclusive    INTEGER NOT NULL DEFAULT 1,
  reason       TEXT,
  created_ts   TEXT NOT NULL,
  expires_ts   TEXT NOT NULL,               -- TTL; expired rows are inert
  released_ts  TEXT                          -- non-NULL once released
);
CREATE INDEX reservation_active ON reservation(agent_name, released_ts, expires_ts);
```

All timestamps are RFC 3339 UTC strings. No foreign-key cascades that could
trigger surprising recursive deletes; cleanup is explicit and bounded.

### D7 — Concurrency & safety model

- **WAL + busy-timeout** per D1. Writers are short single-statement inserts;
  readers are indexed selects. No long-held transactions.
- **Fail-open client (constraint 2):** a thin coordination client wraps the
  store. If opening or querying the DB fails, tools return
  `Response::err(<soft code>, …)` and the agent proceeds — coordination is never
  load-bearing for a turn.
- **Panic-free reads (constraint 2):** no `unwrap`/`expect` on any read path;
  every DB call is `?`-propagated into a soft error. A `#[cfg(test)]`
  in-memory constructor plus a fuzz test over inbox/thread/reservation reads
  guard against a malformed-row panic.
- **Bounded everywhere (constraint 1):** thread reconstruction, inbox fetch,
  conflict checks, and pruning all use SQL with hard `LIMIT`s / capped loops.
  No recursive function walks a thread, a directory, or a reservation set.
- **Safety / trust:** the workspace is the trust boundary (one DB per
  workspace). Reservations are advisory. Name spoofing within a workspace is
  possible by design (agents cooperate within one trust boundary); we do not add
  auth (constraint 5). Path patterns are treated as opaque globs, not resolved
  against the filesystem, so a reservation cannot be used to probe outside the
  workspace.

### D8 — Observability (ADR-006)

Emit an OpenTelemetry span per coordination tool call (`send_message`,
`fetch_inbox`, `mark_read`, `reserve_paths`, …) with **metadata only** —
sender/recipient names, importance, counts, latency, hit/miss — and **never
message bodies or subjects** (privacy-first, matching ADR-006's content-capture
default-off). Analytics may count coordination tool calls like any other tool.

### D9 — Configuration

Add a `[coordination]` table (default-derived like `[kgl]`):

- `enabled = true`
- `busy_timeout_ms = 5000`
- `db_dir` — optional override; default `~/.daimonos/coordination/`
- `default_reservation_ttl_secs`, `max_reservation_ttl_secs`
- `inbox_default_limit`, `thread_max_messages` (the hard thread cap)

### D10 — Verification gates

Implementation of each feature is incomplete until it has:

1. round-trip unit tests against an in-memory store (`open_in_memory`);
2. a concurrency test: two connections to one WAL file, interleaved
   send/fetch, asserting no lost or duplicated delivery;
3. a **fail-open** test: an unopenable/broken store yields a soft error, never a
   panic, and the agent loop continues;
4. a **bounded-thread** test: a long / self-referential reply chain is
   reconstructed within the cap and cannot recurse;
5. a fuzz test over inbox/thread/reservation read decoding proving read paths
   cannot panic on malformed rows;
6. reachability tests proving the tools dispatch through `tool_facade::invoke`
   (internal loop), not only via MCP.

## Amendment — Cooperative unread-mail notifications (#1063)

**Status:** Accepted, 2026-07-25.

Unread mail is surfaced without mutating an active provider stream or tool-call
sequence. Daimonos binds a successfully registered agent name to the live tool
session and checks a bounded metadata-only unread summary at safe generation
boundaries: before the initial provider request and after a complete assistant
`tool_use` + user `tool_result` batch, immediately before the next request.

The model-visible notice is enabled by default and is appended to the ephemeral
system context for that generation, not persisted as a fake user turn. It
contains only unread count, highest importance, agent name, and an instruction
to call `fetch_inbox`; subjects and bodies are never injected automatically.

ACP sessions also run a 1500ms idle poll by default. The poller uses a
non-blocking `AgentSession` lock gate, so ticks during active provider streams
or tool execution are skipped and retried only when idle. It then emits a
clearly labeled, metadata-only `AgentMessageChunk` to Zed so the notice is
immediately visible rather than hidden in collapsed reasoning. The update is
UI-only and is never persisted into provider history. Model and UI delivery use
independent newest-message-id watermarks to prevent repeat spam. Missing
identity or store/query failure disables that check fail-open.

Urgent mail does **not** cancel or interject into an active stream. That more
hazardous behavior is deferred to Vikunja #1064 and remains disabled until a
separate ADR/prototype proves history and tool-result ordering remain valid.

## Consequences

### Positive

- Fleet agents get shared identity, real directed mail (threads/acks/importance),
  and advisory reservations, with **no new daemon, no HTTP, no auth**.
- Reuses the exact WAL/busy-timeout SQLite pattern already proven across
  concurrent daimonos processes (KGL, analytics).
- Single source of truth with zero cross-DB reads structurally avoids the
  reconstruction-on-read crash class that took down agent_mail.
- Coordination faults are isolated to soft, fail-open tool errors.

### Negative

- Multiple processes opening one WAL file means occasional `busy_timeout` waits
  under heavy contention (bounded, not fatal).
- Name uniqueness is per-workspace only; there is no cross-workspace or
  cross-machine identity (out of scope by design).
- Advisory reservations do not prevent a determined clobber (no enforcement
  until a later pre-commit guard).
- A new DB file and `[coordination]` config surface to maintain and migrate.

## Alternatives considered

- **(b) Broker daemon owns the store.** Rejected: forces an always-on process
  the one-shot/chat paths don't guarantee, and converts a coordination bug into
  a fleet-wide crash (violates constraint 2). WAL removes the need.
- **Commingle into an existing DB (KGL or analytics).** Rejected: couples
  migrations/tuning and lets a coordination bug touch unrelated data; a
  dedicated file is cheap.
- **Git-archive-backed store (agent_mail's model).** Rejected outright: it is the
  source of the reconstruction-on-read stack overflow (constraint 1 & 3). Any
  audit trail is write-only export, never read back.
- **execute_script builtins instead of native tools.** Deferred: native
  opcode-facade tools guarantee agent-loop reachability today; sandbox builtins
  can be added later if a scripting need appears.

## References

- Vikunja #1057 (feature spec), #1053 (evaluation + the five design lessons).
- In-repo prior art: `src/kgl/store.rs` (per-workspace WAL SQLite + busy-timeout),
  `src/analytics.rs` (WAL + `synchronous=NORMAL`, read-only connection),
  `src/session_store.rs` (versioned records, atomic writes, safe-id guard),
  `src/main.rs` (per-workspace Unix-socket daemon), `src/tool_facade.rs` /
  `src/ops/mod.rs` (opcode dispatch + in-loop invoke), `src/config.rs`
  (`[kgl]` config shape, `~/.daimonos/` state convention).
- External reference (concepts only, do **not** vendor):
  `Dicklesworthstone/mcp_agent_mail_rust`.
