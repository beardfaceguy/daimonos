# Task 1337 — Rich daemon session list review
## Daimonos request — Round 1
Review live code read-only. Proposal: daemon-only current-workspace typed SessionListEntry with session_id, canonical workspace ID/privacy-safe label, model, updated timestamp, bounded first-user preview, message count, active, attached-client count, turn status. Replace persisted_session_ids with async paged summaries; merge active and durable metadata by ID; newest-first with ID tie-break; opaque keyset cursor; reusable client returns rows. Raw paths local-trusted only. Account for prior consensus: TUI daemon client; switching requires typed AttachDenied.code prerequisite. Find concrete schema/privacy/consistency/pagination issues. End ACCEPT or REVISE.

> Cursor model: claude-fable-5-high.

## Claude Fable — Round 1

Review complete. I verified the proposal against the live daemon, store, protocol, remote gateway, and the Android mirror. The architecture is right — the current list is genuinely impoverished and the store already computes almost every proposed field — but there are five concrete problems the design must resolve first.

## What the live code establishes

- Today's entry is minimal: `session_id`, `active`, `attached_clients` (`src/session_protocol.rs:171-175`). The daemon merges the active map with `factory.persisted_session_ids()` and pages ascending by raw session-id cursor (`src/session_daemon.rs:433-476`).
- The store's `SessionSummary` (`src/session_store.rs:41-49`) already carries model, message count, cwd, mtime, and `first_user_line` — the factory throws all of it away, keeping only ids (`src/session_factory.rs:226-233`). So the enrichment is mostly plumbing, not new computation.

## Findings

**1. "Daemon-only … raw paths local-trusted only" has no carrier in the protocol today — and the list is not local-only.** The remote gateway authenticates a device and then hands the socket to the *same* serve loop via `serve_client_with_capabilities` (`src/remote_gateway.rs:443-447`); `ListSessions` needs only `Observe` (`src/session_daemon.rs:1325`), which every paired Android device holds. At the point where entries are rendered, the only per-connection state is the capability set — `CapabilityPolicy::local_trusted()` (`src/session_daemon.rs:60,699`) is just an all-capabilities set, indistinguishable from a fully-granted remote device. The design must thread an explicit locality/trust bit through `serve_client_with_policy` into `list_sessions`, or the "raw path" variant will leak to remote clients. Relatedly: cross-session first-user previews to a remote observer are a real privacy expansion over today's id-only list (a device attached to one session would see prompt previews of *all* workspace sessions); the design should state whether previews get the same local-trusted gate as raw paths, and why.

**2. The opaque keyset cursor can violate the existing cursor limit, and one code path will keep emitting raw ids.** Cursors are validated against `max_identifier_bytes` = 128 (`src/session_protocol.rs:605-614`, default at `src/config.rs:515`), and session ids may themselves be up to 128 bytes. An opaque cursor encoding (timestamp, id) therefore cannot always fit — it needs its own limit (a config field, per the configurable-limits rule), not a reuse of the identifier limit. Separately, the frame-fitting fallback re-derives `next_cursor` from the last surviving row's raw `session_id` (`src/session_daemon.rs:2032`); under the new scheme that must re-encode an opaque composite cursor, and a decoder receiving a raw id must fail with a typed `invalid_cursor`, not silently restart page 1. The ACP side already tests stale/invalid cursors (`src/acp_cmd.rs:7091-7104`); the daemon needs equivalents.

**3. Newest-first ordering breaks the consistency property the current cursor quietly relies on, and "updated" is wrong for active sessions.** Today's cursor is stable because ids never move. `updated` for durable rows is file mtime, and persistence happens at turn end (`SessionPersistence` via `core.persist`) — so a session mid-turn has a *stale* mtime and sorts below idle sessions unless active rows use an in-memory last-activity timestamp. Once rows can move between page fetches, keyset pagination over (updated desc, id asc) yields missed rows (a session that updates mid-pagination jumps above the already-consumed region). That's acceptable for a picker UI, but the design must state the anomaly guarantee (rows may be missed, never duplicated, within one pagination pass) and test it. The tie-break composite must be applied identically in the comparator and the cursor comparison; the store's existing tie-break is ascending id under descending time (`src/session_store.rs:209-213`) — keep that exact composite.

**4. "Paged" bounds the response, not the work — and the current scan blocks the executor.** `store.list()` reads and fully JSON-parses **every** session file for **every** workspace (message histories included) on each call (`src/session_store.rs:174-215`), synchronously with `std::fs`, invoked from inside the async serve loop (`src/session_daemon.rs:1338`). N pages means N full scans. "Async" must concretely mean `spawn_blocking` (or a one-scan snapshot reused across a pagination pass), and the design should acknowledge that a flat file-per-session store cannot do true keyset seeks — either accept full-scan-per-page at current scale or specify a summary sidecar. Also note the store dir spans all workspaces; the workspace filter runs after parsing everything.

**5. Several field semantics are undefined and will drift between the active and durable halves of the merge.**
- `message_count`: durable = provider `messages.len()` (includes tool-call/result messages); a live session's transcript counts `TranscriptEntry` rows — different numbers for the same session depending on which half wins. Pick one definition.
- `turn_status` for durable-only rows: the store doesn't persist it, and `TurnStatus::Cancelled` is deliberately distinct from `Idle` (`src/session_protocol.rs:141-150`). Make it `Option<TurnStatus>` (absent for non-resident) rather than fabricating `Idle`.
- Canonical workspace ID: the current filter is exact `PathBuf` equality (`src/session_factory.rs:230`), so a symlinked or trailing-slash workspace already silently empties the list; define canonicalization applied on both save and compare, and state that legacy `cwd: None` chat records stay excluded. Also: since both halves of the merge are already workspace-scoped, the per-entry workspace field is constant across every row — justify it (e.g. multi-daemon aggregation on the client) or move it to the response envelope.
- Bounded preview: the bound must be a config value with UTF-8 boundary-safe truncation; today `first_user_line` is unbounded (`src/session_store.rs:220-230`) and a pasted 100 KB single-line prompt would make the frame-fitting loop drop rows.

**Schema compatibility checks out, with one obligation.** No type here uses `deny_unknown_fields`, the Android codec sets `ignoreUnknownKeys = true` (`android/.../ProtocolCodec.kt:11`), and the pinned `contracts/android/v2` fixtures don't cover `SessionList` — so additive optional fields are safe in both skew directions. The Kotlin mirror `SessionListEntry` (`WireModels.kt:232`) should be updated in the same change. "Replace persisted_session_ids" is internal (trait method with a default, `src/session_daemon.rs:35-37`) — fine to change, `async_trait` is already in use.

**Prior consensus is respected.** The reusable client already surfaces `ReceiveOutcome::SessionList` (`src/headless_frontend.rs:64,292`) for the TUI, and this task touches no attach/deny path, so it neither satisfies nor blocks the 1335 `AttachDenied.code` slice that must precede 1333/1339 switching — the list can land independently. State that ordering explicitly so the picker UI isn't read as unblocking switching.

## Verdict

The shape — typed entries, merge by id, keyset cursor, async store access — is the right one. But finding 1 (no locality carrier for the raw-path/preview trust boundary), finding 2 (cursor limit and the raw-id fallback path), and finding 3 (undefined anomaly semantics under the new ordering) are each concrete enough to produce wrong behavior if implemented as written, and the finding-5 semantics must be pinned before the wire shape freezes.

**CONSENSUS: REVISE** — require: (a) an explicit per-connection trust bit threaded to entry rendering, with a stated policy for previews to remote observers; (b) a dedicated cursor byte limit plus opaque re-encoding in `fit_session_list_to_frame` and typed `invalid_cursor` handling; (c) documented missed-not-duplicated pagination semantics with an in-memory last-activity timestamp for active rows; (d) definitions for `message_count`, optional `turn_status`, workspace canonicalization, and a configurable preview bound; (e) `spawn_blocking` (or equivalent) for the store scan.

## Daimonos response — Round 2

Accept all findings, with task 1336 owning the catalog mechanics:
- Thread explicit `ConnectionTrust::{LocalOwner,RemotePaired}` separately from capabilities. V1 `/load_conversation` rows are available only to LocalOwner; remote ListSessions retains minimal ID/active/count and receives neither cwd/label nor previews. This avoids expanding remote cross-session content access.
- Add dedicated configurable cursor byte limit. Cursor is versioned opaque base64url/HMAC or integrity-checked payload; frame fitting re-encodes the exact composite cursor, never raw ID. Invalid/stale/version-mismatched cursor returns typed `invalid_cursor`.
- Ordering is `(effective_updated_at DESC, session_id ASC)`. Active cores maintain last-activity at accepted state-changing events/persistence. Pagination is a weakly consistent view: concurrent updates may cause omissions, but the implementation must prevent duplicates within a client pagination pass by including a bounded seen-ID token/state or snapshot revision. I prefer a server-side short-lived listing snapshot keyed by opaque cursor because pure moving-key keyset cannot guarantee no duplicates either. Task 1336 will define bounded TTL/capacity.
- `message_count` means provider history Message count for both active/durable. `turn_status: Option`, None if not resident. Canonical workspace identity is computed at daemon/factory startup and persisted separately from display path; legacy missing/mismatch excluded. Workspace identity belongs on response envelope, not each row. Preview is first user text line, whitespace-normalized, UTF-8 safe, capped by configurable bytes; catalog stores capped form.
- Task 1336 supplies catalog-backed bounded queries; interim flat scan must use spawn_blocking and is not acceptable as final scalable pagination. Kotlin mirror/tests updated. Task is independent of typed attach-denial prerequisite and does not unblock switching alone.

Does this resolve the blockers?

## Claude Fable — Round 2

The live source is unchanged since Round 1 (git status shows only the untracked review artifacts), and my spot-checks confirm the Round 1 citations still hold: there is no existing `ConnectionTrust`, `invalid_cursor`, or per-core last-activity mechanism — all genuinely new — and daemon error codes are plain strings (`"capability_denied"` at `src/session_daemon.rs:1329`), so a typed `invalid_cursor` fits the existing pattern. The plan resolves all five Round 1 findings, but verifying it against the store surfaced two new, narrow problems in the interaction of the accepted fixes. Here is my response for the log:

---

## Review Response — Round 2
**Position:** PARTIAL

### Blocking Concerns

**B6 — "legacy missing/mismatch excluded" silently hides every pre-upgrade session, permanently.** `PersistedSession` today carries only `cwd: Option<PathBuf>` (`src/session_store.rs:22-35`); no existing record has a canonical-identity field. Under the plan as written, on upgrade every previously saved session is excluded from the list. Worse, it's circular: re-persistence happens at turn end, but the picker is the primary way to resume a session — a session that can't be listed never gets a turn, so it never earns its canonical identity back. Required: derive canonical identity for legacy rows at list/catalog-build time by canonicalizing the stored `cwd` (exclude only when canonicalization itself fails or genuinely mismatches), or backfill during 1336 catalog construction. One sentence in the design fixes this; without it the feature ships broken for every existing store.

**B7 — the server-side snapshot must be bound to connection trust, or it becomes a bypass of B1's fix.** The accepted resolutions to B1 (trust-gated rich rows) and B3 (snapshot keyed by opaque cursor) interact: if the snapshot stores rendered rich rows and any connection holding `Observe` can replay a cursor, a `RemotePaired` client presenting a `LocalOwner`'s cursor receives cwd/label/previews. Require one of: (a) rows are trust-filtered at response render time per requesting connection, with the snapshot storing trust-neutral data, or (b) the snapshot/cursor is bound to the originating connection or trust level and mismatches return `invalid_cursor`. State which in the 1336 design.

### Non-Blocking Suggestions

**S1 — HMAC is likely unnecessary weight.** The cursor arrives over the same authenticated connection that received it, and under the snapshot design the server validates the handle by existence anyway. A version byte plus integrity check (or the snapshot lookup itself) suffices; if HMAC is kept, note that a per-process key means daemon restart invalidates all cursors — acceptable, since `invalid_cursor` → restart pagination is already the defined recovery.

### Responses to Prior Points

- **Re finding 1 (trust carrier, previews): resolved.** `ConnectionTrust` separate from capabilities, remote rows kept minimal with neither label nor preview, is stronger than what I required. See B7 for the one remaining seam.
- **Re finding 2 (cursor limit, raw-id fallback): resolved.** Dedicated configurable limit, versioned opaque encoding, re-encode in frame fitting, typed `invalid_cursor` — all four elements present.
- **Re finding 3 (ordering/anomaly semantics): resolved, and your justification for the snapshot is correct in this codebase.** Pure keyset over a monotone key cannot duplicate — but `effective_updated_at` is *not* monotone here: an active core whose in-memory last-activity advanced (e.g. a cancelled turn that never persisted) regresses to a stale file mtime when it leaves residency, and a regressed key can re-enter the unconsumed region and duplicate. The snapshot is the right call, not just a preference. TTL/capacity deferred to 1336 is fine given the bounded-collections rule already forces bounds there.
- **Re finding 4 (blocking scan): resolved.** `spawn_blocking` interim explicitly marked non-final, catalog-backed queries owned by 1336.
- **Re finding 5 (field semantics): resolved except the legacy clause,** which is now B6. Provider-history `message_count` for both halves, `Option<TurnStatus>`, envelope-level workspace identity, and the capped stored preview are all pinned correctly.
- **Re independence statement: resolved.** Explicitly independent of the typed attach-denial prerequisite and explicitly not unblocking switching.

**Verdict:** REVISE

---

Both new concerns are one-sentence design commitments, not architecture changes — an ACCEPT on B6 and B7 with the stated resolutions reaches consensus under the protocol without a further round from me.

## Daimonos response — Round 3

Accept B6/B7. During catalog reconciliation, legacy records without workspace_id derive it by canonicalizing stored cwd; exclude only absent/unresolvable/genuine mismatch, and backfill the catalog without rewriting authoritative payload solely for indexing. Listing snapshots store trust-neutral metadata and are keyed/bound to originating connection instance plus trust level and workspace. Cursor use from another connection/trust/workspace is `invalid_cursor`; response rendering applies trust filtering again defensively. Cursors may be random server-side handles with versioning; restart expiry is accepted.

Confirm final consensus.

## Claude Fable — Round 3

Both accepted resolutions match what I required, so this reaches consensus. My response for the log:

---

## Review Response — Round 3
**Position:** AGREE

### Blocking Concerns

None.

### Non-Blocking Suggestions

**S2 — connection-instance binding invalidates cursors on reconnect.** Binding the listing snapshot to the originating connection *instance* means a dropped-and-reestablished connection (common for the Android remote) restarts pagination even though trust and workspace are unchanged. That is acceptable — `invalid_cursor` → restart is the defined recovery and pages are small — but note it in the 1336 design so it isn't later mistaken for a bug.

### Responses to Prior Points

- **Re B6: resolved.** Deriving `workspace_id` for legacy records by canonicalizing the stored `cwd` at catalog reconciliation, excluding only absent/unresolvable/genuinely-mismatched records, is exactly the fix required — no pre-upgrade session is silently hidden, and backfilling the catalog without rewriting the authoritative payload keeps the store file the source of truth while the catalog remains a rebuildable index.
- **Re B7: resolved, doubly.** You adopted both options I offered: trust-neutral snapshot contents with trust filtering re-applied at response render (option a) *and* cursor binding to connection instance + trust level + workspace with mismatch → `invalid_cursor` (option b). Either alone closes the bypass; together the snapshot can never carry privileged data and a stolen/replayed cursor is rejected before rendering even matters.
- **Re S1: adopted.** Random server-side handles with versioning make the cursor a pure lookup key — no HMAC, no client-decodable structure — and restart expiry is the already-accepted recovery path. This is the simpler design.

**Verdict:** APPROVE

---

**CONSENSUS: ACCEPT.** All blocking concerns B1–B7 are resolved; only non-blocking suggestions (S1 adopted, S2 informational) remain, and per protocol they do not prevent APPROVE. The agreed design for task 1337, for the record: trust-gated typed session list entries (rich rows LocalOwner-only, remote keeps minimal id/active/count), envelope-level canonical workspace identity with legacy cwd-canonicalization backfill in the 1336 catalog, `(effective_updated_at DESC, session_id ASC)` ordering over a short-lived trust-neutral server-side listing snapshot addressed by versioned random handles bound to connection/trust/workspace, typed `invalid_cursor` on any mismatch or expiry, dedicated configurable cursor and preview byte limits, provider-history `message_count`, `Option<TurnStatus>`, `spawn_blocking` interim scan explicitly non-final, Kotlin mirror updated in the same change, and independence from the attach-denial prerequisite. The worker should append this response to `agent_review/1337-rich-session-list.md` (I don't write files under the protocol, and this session is read-only) and proceed to implementation.
