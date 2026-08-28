# Task 1336 — Bounded session discovery review, Round 1
Review live code read-only. Proposal: SQLite metadata catalog beside daemon-sessions, JSON remains authoritative. After atomic JSON save, catalog upsert via spawn_blocking behind bounded semaphore. Indexed workspace_id, updated desc, ID asc. Listing uses short-lived bounded server-side snapshot and random opaque cursors bound to connection/trust/workspace (per 1337 consensus), page_size+1, cancellation/timeouts. Attach always validates JSON. Bounded rate-limited reconciliation with persisted cursor; no unbounded startup rebuild. Schema-version catalog, rebuild never modifies payload. Legacy workspace identity derived by canonicalizing stored cwd. Find crash consistency, concurrency, SQLite/runtime, bounding, pagination problems. End ACCEPT or REVISE.

> Model: claude-fable-5-high.

## Claude Fable — Round 1

Review complete. I verified the proposal against the live store, persistence layer, factory, daemon, and the existing SQLite stores (analytics, coordination, KGL). The architecture is sound — a rebuildable SQLite index beside an authoritative JSON store is the right shape, and it honors the 1337 consensus — but six concrete problems surfaced, two of them in facts the proposal doesn't account for: the store directory is shared by multiple daemon processes, and the save path it hooks is synchronous and error-swallowing.

What the live code establishes:

- The daemon socket is per-workspace — hashed canonical workspace path (`src/config.rs:645-658`) — but `daemon-sessions` is one global directory (`src/paths.rs:33-35`, wired at `src/agent_runtime.rs:516`). Multiple daemon processes routinely share the store, and would share the catalog.
- All daemon saves and deletes route through `SessionPersistence` (`src/session_core.rs:527-581`); `core.persist` is a synchronous fn (`src/session_core.rs:861`), and `save_record` swallows write errors (`src/session_store.rs:135-137`). Deletion is live: `end_session(delete_persisted)` (`src/session_daemon.rs:559-620`) and the eviction path (`src/session_daemon.rs:258`).
- Existing SQLite stores use WAL + `synchronous=NORMAL` + configurable `busy_timeout_ms` (`src/coordination/store.rs:158-178`, `src/kgl/store.rs:72-90`), and analytics already solved detached-write loss with a tracked `pending_writes` drain (`src/analytics.rs:454-493`, motivated at `src/mcp.rs:360-364`).

---

## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns

**B1 — The catalog is a multi-process database, and the proposal treats it as daemon-private.** One daemon per workspace, one global store dir: two live daemons (or, after an upgrade, an old-binary and a new-binary daemon) write the same catalog concurrently. Three consequences the design must pin. (a) Open with the existing WAL/`synchronous=NORMAL`/configurable-busy-timeout pattern — multi-process SQLite is fine, but only with those pragmas. (b) The schema-version policy must be **asymmetric**: an *older* version → migrate or drop-and-rebuild; a *newer/unknown* version → leave the file untouched and fall back to the interim scan. A symmetric "unknown → rebuild" rule makes two binary versions destroy each other's catalog in a loop. (c) The persisted reconciliation cursor is shared state — define whether it's one row (N reconcilers advance it jointly; aggregate rate is N× the configured limit) or per-daemon keyed by workspace_id. Either works; undefined doesn't.

**B2 — The write path as proposed can regress the catalog and can index writes that never happened.** Four interacting facts: JSON writes per session are serialized under the persistence-state mutex (`src/session_core.rs:556-571`), but detached `spawn_blocking` upserts complete in arbitrary order — save A then save B can apply as B then A, leaving stale metadata and a regressed sort key that mtime comparison can't reliably repair (mtime granularity ties). `persist` is sync and cannot await the semaphore, so saturation means either try-acquire-and-drop (silent staleness until a rate-limited reconciliation wraparound) or spawned waiters (unbounded accumulation, violating the bounded-collections rule). And `save_record` returns no success signal — an unconditional post-save upsert can index a failed write. Required, as one coherent fix: surface `write_atomic`'s result and the post-rename file mtime to the hook (upsert only on success, `updated` = actual mtime so reconciliation can equality-compare); a per-session monotonic generation carried into the upsert with a `WHERE generation >= existing` guard, or equivalently a bounded per-session latest-wins pending map that both coalesces bursts and defines saturation. The coalescing map solves ordering and saturation together.

**B3 — Deletion is absent from the proposal, and the reconciliation keyspace is undefined.** The proposal hooks only "after atomic JSON save," but deletion is a live path today. A crash between file unlink and catalog delete leaves ghost rows that list but fail attach — acceptable only if reconciliation removes them, which requires reconciliation to be **bidirectional**: catalog→dir (stat by id to find ghosts) and dir→catalog (find missing/stale rows). Note also that readdir order is unstable, so a persisted cursor over raw directory enumeration is unsound; the cursor must walk a stable keyspace (the catalog's id index, plus a bounded name-only directory sweep for discovery). State the wraparound latency bound the rate limit implies.

**B4 — Legacy canonicalized identity contradicts the live attach validation.** Attach compares the stored cwd to the factory workspace by raw `PathBuf` equality (`src/session_factory.rs:83-90`). Deriving catalog workspace_id by canonicalizing stored cwd means a workspace reached via symlink or alias *lists* legacy sessions that attach then rejects with "belongs to a different workspace" — and the socket path already canonicalizes (`src/config.rs:650-651`), so this alias scenario reaches one daemon today. "Attach always validates JSON" must mean validating with the *same* canonical comparison the catalog uses, in the same change.

**B5 — Cold-start and rebuild completeness semantics are undefined.** "No unbounded startup rebuild" plus rate-limited reconciliation means that on first upgrade, or after catalog loss/corruption/schema bump, the catalog is empty and the picker silently shows nothing (or only resident sessions) until reconciliation wraps. For the primary discovery surface that reads as data loss. Require a stated policy: while the catalog is marked incomplete for a workspace, serve listing from the interim `spawn_blocking` scan (already required by 1337) or run a bounded synchronous backfill (configurable file/time cap) on first list — either is fine; silent incompleteness is not.

**B6 — The snapshot store's bounds and lifecycle were delegated here by 1337 and are still unspecified.** "Short-lived bounded" needs numbers and policies: per-connection and global snapshot capacity, TTL, max rows per snapshot (trust-neutral id+key rows are ~tens of bytes, so a generous cap is cheap), behavior at capacity (evict-oldest is clean — the victim's cursor becomes the already-defined `invalid_cursor` recovery), and — per the resource-lifecycle rule — reaping on connection drop, not TTL alone, since cursors are bound to the connection instance anyway. All limits in config + `daimonos.default.toml` per the configurable-limits rule.

### Non-Blocking Suggestions

**S1 — Reuse the analytics drain pattern at shutdown.** The codebase already documented that bare `spawn_blocking` writes are dropped on exit (`src/mcp.rs:360-364`) and built `pending_writes`/`wait_until_quiet` for it. `daemon.shutdown()` persists every session and already has a `shutdown_grace` window (`src/session_daemon.rs:643-660`) — drain pending catalog upserts inside it, or the freshest saves are exactly the ones stale at next startup.

**S2 — Scope the hook so non-daemon stores don't grow catalogs.** `SessionPersistence` is also constructed by the standalone ACP engine against the separate agent-sessions dir (`src/acp_cmd.rs:2245`; `src/paths.rs` asserts the dirs differ). Make the catalog handle an optional injection wired only in daemon startup, not an unconditional behavior of `SessionPersistence` or `SessionStore`.

**S3 — Listing timeouts must account for non-cancellable blocking work.** A timed-out `spawn_blocking` query keeps running and keeps its semaphore permit until SQLite returns; with a shared `Mutex<Connection>` a slow checkpoint can queue several abandoned queries. Size the timeout above `busy_timeout_ms` and note the permit-held window in the design.

**Verdict:** REVISE

---

The core decisions — JSON authoritative, catalog rebuildable and never touching payloads, snapshot-based pagination with connection/trust/workspace-bound handles, attach-time JSON validation — are all correct and consistent with the 1337 consensus. B2 and B4 will produce observably wrong behavior if implemented as summarized; B1, B3, B5, B6 are unstated policies that each have a cheap resolution. Per the protocol I don't write files — append this response to `agent_review/1336-bounded-session-discovery.md` verbatim.

**REVISE**

## Daimonos response — Round 2

Accept B1-B6 and S1-S3. Revised design:
- Shared multi-process SQLite uses WAL, NORMAL, configurable busy timeout. Older schema is migrated/rebuilt under lock; newer unknown schema is untouched and that binary uses spawn_blocking scan. Reconciliation state is per workspace/daemon instance with a global catalog lease/rate budget so aggregate work is bounded.
- Refactor atomic save to return success + actual post-rename mtime. Inject optional daemon-only catalog writer. A bounded latest-wins pending map keyed session ID stores monotonic generation; workers coalesce and SQL upsert applies only generation >= stored. Saturation replaces same-key work; distinct-key hard cap marks catalog incomplete and reconciliation repairs. Drain pending work within shutdown grace.
- Successful delete enqueues tombstone generation. Reconcile bidirectionally: catalog IDs are scanned by stable key; directory discovery uses bounded sorted filename batches/snapshot and wraparound. Ghosts removed only after confirming payload absent. Publish/document worst-case reconciliation latency.
- Factory attach uses the same canonical workspace identity comparison as catalog, with safe fallback/errors when canonicalization fails.
- If catalog incomplete/cold/unsupported, first/list requests use bounded spawn_blocking fallback scan and return an explicit partial/incomplete marker when configured cap/time is exceeded; never silently empty.
- Snapshot limits are configurable: per connection, global, TTL, max rows/bytes; oldest eviction => invalid_cursor; drop all connection-bound snapshots on disconnect. Timeout exceeds busy timeout; semaphore permit remains held until blocking query completes.

Potential concern: a fully bounded fallback scan cannot guarantee a complete global newest-first page without examining every filename. Therefore while incomplete it should return `incomplete=true` and a deterministic bounded best-effort page, not claim completeness; reconciliation progressively establishes full catalog. This is preferable to blocking unboundedly. Any material blocker remains?

## Claude Fable — Round 2

Verification is complete. The live code is unchanged since Round 1, all my citations still hold, and the revised design genuinely resolves B1–B6 — but checking the new mechanics against the actual write and delete paths (`SessionPersistence::save`/`delete` at `src/session_core.rs:547-581`, `SessionStore::delete` at `src/session_store.rs:161-170`) surfaced two narrow problems in how the accepted fixes interact. Both are one-sentence commitments, same as the 1337 Round 2 pattern. Here is my response for the log:

---

## Review Response — Round 2
**Position:** PARTIAL

### Blocking Concerns

**B7 — The generation guard dies across daemon restart.** The per-session monotonic generation lives in the in-memory pending map, but the catalog persists it for the `generation >= stored` comparison. A restarted daemon's counters restart from zero, so every fresh save for a session whose catalog row carries a prior instance's higher generation is silently rejected by the guard — the primary write path is dead after every restart, and the catalog's sort keys go stale exactly for the actively-used sessions, until reconciliation happens to visit each one (a latency you've committed to publishing precisely because it can be long). This resurrects the B2 staleness failure through the B2 fix itself. Required, either: seed the in-memory generation for a session from the stored catalog row on first pending-map insertion (one read, amortized), or scope the guard by a daemon-instance ID stored alongside generation and accept cross-instance writes when the actual mtime advances. Note the same seeding must cover tombstone generations, or a delete issued right after restart is equally rejected.

**B8 — A plain SQL DELETE for tombstones lets an in-flight stale upsert resurrect a ghost.** "Workers coalesce" implies multiple appliers. The latest-wins map only coalesces entries still *in* the map: if worker 1 has already dequeued the generation-N upsert for session K and is mid-flight when the delete enqueues the generation-N+1 tombstone, worker 2 can apply the DELETE first, after which worker 1's upsert finds no stored generation to compare against and inserts a row for an unlinked file — a ghost that lists but fails attach, the exact B3 failure class, present until reconciliation confirms payload absence. Required, either: tombstones are materialized rows (deleted flag + generation) that the upsert guard compares against, purged only by reconciliation after confirming the payload is absent; or same-key application is serialized (at most one in-flight apply per session ID, re-checking the map on completion). The materialized tombstone is the more robust choice since it also covers the multi-worker delete-then-rapid-recreate ordering.

### Non-Blocking Suggestions

**S4 — Propagate the incomplete marker through the 1337 listing snapshot.** A snapshot built while the catalog is incomplete (or from the fallback scan) serves multiple subsequent pages; the `incomplete=true` marker must ride on every page served from that snapshot, not only the first response, or page 2+ silently claims completeness the snapshot never had.

### Responses to Prior Points

- **Re B1: resolved.** WAL/NORMAL/configurable busy timeout, asymmetric schema policy (older migrated under lock, newer untouched with that binary falling back to scan), and per-workspace/instance reconciliation state under a global lease/rate budget — all three sub-points pinned. The lease living in the shared DB is the only coordination medium available and is the right one.
- **Re B2: resolved, modulo B7.** Success + actual post-rename mtime surfaced from the save, daemon-only injected writer, bounded latest-wins map with generation guard, defined saturation (same-key replacement; distinct-key cap marks the catalog incomplete for reconciliation repair) — this is the coherent fix I required. B7 is the one seam the restart boundary opens in it.
- **Re B3: resolved, modulo B8.** Bidirectional reconciliation over the catalog's stable ID keyspace plus bounded sorted-filename directory batches is sound — sorted-name batching is O(dir) per batch but bounded in memory, which is the right trade. Confirming payload absence before ghost removal closes the reconciler-vs-concurrent-save race. Publishing worst-case wraparound latency satisfies the bound-statement requirement.
- **Re B4: resolved.** Same canonical comparison in factory attach as in the catalog, with defined behavior on canonicalization failure, matches the 1337 B6 consensus.
- **Re B5 and your self-raised concern: resolved — and your concern is correctly decided.** A bounded scan mathematically cannot certify a global newest-first page without examining every filename; `incomplete=true` with a deterministic bounded best-effort page is exactly what "never silently empty, never falsely complete" requires, and progressive establishment via reconciliation is the right recovery. This is not a blocker; see S4 for the one propagation obligation it creates.
- **Re B6: resolved.** All limits configurable, oldest-eviction mapping to the already-defined `invalid_cursor` recovery, disconnect reaping per the resource-lifecycle rule, timeout sized above busy timeout with the permit-held window acknowledged.
- **Re S1–S3: adopted** (shutdown-grace drain, daemon-only injection, timeout/permit semantics).

**Verdict:** REVISE

---

To answer the closing question directly: your self-raised fallback-scan concern is **not** a material blocker — the `incomplete=true` best-effort semantics are the correct resolution. The two remaining blockers are B7 and B8, both narrow interaction bugs in the accepted B2/B3 machinery with stated one-sentence fixes; an ACCEPT on both with those commitments reaches consensus under the protocol without a further round from me. Per the protocol (and this session being read-only) I don't write files — append this response to `agent_review/1336-bounded-session-discovery.md` verbatim.

**REVISE**

## Daimonos response — Round 3

Accept B7/B8/S4. Use materialized catalog rows with `(writer_instance_id, generation, deleted)` and actual mtime. On first mutation per session, seed generation from the current row under the SQL transaction; within one writer instance accept increasing generation, while a different instance may supersede only when authoritative payload observation (post-save mtime/file state) is newer. Tombstones remain rows and block stale upserts; purge only after bounded reconciliation confirms absence and retention age. Same-key DB mutation is additionally serialized by transaction/row logic, but correctness does not rely solely on in-memory serialization. Rapid recreate must observe a newer successful payload mtime/state and supersede tombstone. Every page from an incomplete listing snapshot carries incomplete=true.

Confirm consensus or identify unresolved issue.

## Claude Fable — Round 3

I'm the reviewer on this log, and this is the worker's final-round acceptance of B7/B8/S4 with concrete mechanics. The working tree is unchanged since Round 1 (git status shows only the untracked `agent_review/` and `docs/review-protocol.md`), so all prior citations stand. I evaluated the Round 3 mechanics against the two failure scenarios that motivated B7 and B8; both are closed. Consensus is reached. Here is my response for the log:

---

## Review Response — Round 3
**Position:** AGREE

### Blocking Concerns

None.

### Non-Blocking Suggestions

**S5 — Equal-mtime cross-instance tie.** The rule "a different instance may supersede only when the observed payload mtime is newer" has one narrow gap: two writes to the same session from different instances within the filesystem's mtime granularity produce an equal mtime, and the strictly-newer test rejects the genuinely later write (this includes a cross-instance rapid-recreate landing with mtime equal to the tombstone's recorded observation). On ext4 with nanosecond timestamps the window is negligible, and bidirectional reconciliation repairs the residue because the dir→catalog sweep observes the file's true state — so this is not blocking. Worth one decision in the implementation: either accept reconciliation as the repair, or admit equal-mtime supersede with a documented tiebreak. Do not silently assume strict inequality can't tie.

### Responses to Prior Points

- **Re B7: resolved.** Seeding the generation from the current row *inside the SQL transaction* on first mutation per session eliminates the restart-death: a fresh daemon continues from the persisted generation rather than restarting at zero, with no read-then-write window because the seed and the guarded mutation share the transaction. The `(writer_instance_id, generation)` split plus mtime-gated cross-instance supersede also covers the case B7's second option targeted — two live instances on the shared store — and since both instances stat files on the same filesystem, the mtimes they compare come from the same kernel clock, so the cross-instance comparison is coherent. Tombstones being rows with generations means the post-restart-delete case is covered by the same seeding.
- **Re B8: resolved.** Materialized tombstones are exactly the required fix: the mid-flight stale upsert now finds a row with a `deleted` flag and a generation to lose against, instead of an absence it can insert over — the ghost-resurrection path is gone. Purge gated on both reconciliation-confirmed payload absence *and* retention age closes the purge-vs-in-flight-recreate window. Making transaction/row logic the correctness boundary, with in-memory serialization only an optimization, is the right dependency direction. Rapid recreate superseding via newer payload observation (or, same-instance, via increasing generation) handles the delete-then-recreate ordering I flagged; the one residual tie is S5, non-blocking.
- **Re S4: adopted.** Incomplete marker rides every page served from an incomplete snapshot.

**Verdict:** APPROVE

---

Consensus per the protocol: no unresolved blocking concerns remain across B1–B8; S1–S5 are recorded as non-blocking and the worker proceeds. Per the protocol (and this session being read-only) I don't write files — append this response to `agent_review/1336-bounded-session-discovery.md` verbatim.

**ACCEPT**
