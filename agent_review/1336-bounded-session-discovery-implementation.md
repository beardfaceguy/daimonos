# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1336-bounded-session-discovery-implementation-20260827" -->

<!-- event id="request" artifact path="1336-bounded-session-discovery-implementation/artifacts/round-1-review-request.diff" sha256="4c813b8e063863d55e3c81c91ddfa6da07443f50fc0ad5800628f147657e6153" -->
## Review Request — Round 1
**Task:** 1336 — Bounded multiprocess session discovery catalog with 1337 regression audit
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Keep daemon-session JSON authoritative while adding a rebuildable shared SQLite metadata catalog. Use WAL/NORMAL/configurable busy timeout and asymmetric schema handling; daemon-only bounded latest-wins mutation writer; actual post-rename mtime; unique atomic temp files; materialized generation-guarded tombstones; bounded query permits retained by timed-out blocking work; shared reconciliation lease and stable directory/catalog cursors; bidirectional repair and retained tombstone purge; bounded incomplete fallback with incomplete=true on every frozen listing page; bounded global connection-snapshot registry. Preserve task 1337 trust filtering, rich-row semantics, opaque cursor behavior, client/TUI propagation, and Android mirror.

### Relevant Code / Diff
Review the actual current uncommitted diff. Load-bearing implementation excerpts follow.

```rust
// src/session_catalog.rs
const SCHEMA_VERSION: i64 = 1;

pub enum CatalogOpen {
    Ready(SessionCatalog),
    NewerSchema { found: i64 },
}

pub struct CatalogMutation {
    pub session_id: String,
    pub workspace_id: String,
    pub model: Option<String>,
    pub updated_at_unix_ns: u64,
    pub preview: Option<String>,
    pub message_count: Option<usize>,
    pub writer_instance_id: String,
    pub generation: u64,
    pub deleted: bool,
    pub observed_at_unix_ms: u64,
    pub authoritative_observation: bool,
}

// Open checks schema_meta before applying WAL pragmas. A newer schema returns
// NewerSchema without touching DB content/journal mode. Current/older schemas
// initialize under BEGIN IMMEDIATE. Older catalog tables are dropped/rebuilt;
// authoritative JSON files are outside the DB and untouched. Ready connections
// use WAL, synchronous=NORMAL, configurable busy timeout, owner-only parent and
// mode-0600 catalog file.

// session_catalog columns: session_id PK, workspace_id, model, updated ns,
// preview, count, writer_instance_id, generation, deleted, observed timestamp.
// workspace_state: per-workspace complete + stable reconciliation cursor.
// reconcile_lease: one global owner/expiry row.

pub fn apply(&self, mutation: &CatalogMutation) -> Result<bool> {
    // INSERT ... ON CONFLICT DO UPDATE WHERE:
    // same writer_instance_id && increasing generation
    // OR different writer and newer actual payload mtime
    // OR equal mtime where a tombstone supersedes a live row
    // OR authoritative_observation from reconciliation.
}

pub fn rows(&self, workspace, limit) -> Result<Vec<CatalogRow>> {
    // WHERE workspace_id=? AND deleted=0
    // ORDER BY updated_at_unix_ns DESC, session_id ASC LIMIT ?
}

pub fn ids_after(&self, workspace, after, limit) -> Result<Vec<String>> {
    // stable session-id scan over nondeleted catalog rows
}

pub fn try_acquire_reconcile_lease(&self, owner, now, interval) -> Result<bool> {
    // BEGIN IMMEDIATE; only replace expired global lease.
}
```

```rust
// bounded daemon-only writer in src/session_catalog.rs
pub struct SessionCatalogWriter {
    catalog: SessionCatalog,
    workspace_id: String,
    writer_instance_id: String,
    max_pending: usize,
    batch_size: usize,
    max_preview_bytes: usize,
    generation: AtomicU64,
    pending: Mutex<PendingState>, // BTreeMap<session_id, latest mutation>
    wake: mpsc::Sender<()>,       // capacity 1
    in_flight: AtomicUsize,
    healthy: AtomicBool,
    quiet: Notify,
}

fn enqueue(&self, mutation: CatalogMutation) {
    if !self.is_healthy() { return; }
    let mut pending = self.pending.lock()...;
    if !pending.mutations.contains_key(&mutation.session_id)
        && pending.mutations.len() >= self.max_pending {
        pending.mark_incomplete = true; // distinct-key saturation
    } else {
        pending.mutations.insert(mutation.session_id.clone(), mutation); // coalesce
    }
    drop(pending);
    let _ = self.wake.try_send(());
}

async fn run_writer(weak_writer, receiver) {
    // Upgrade Weak only while processing, avoiding an Arc/task/channel cycle.
    // Remove at most batch_size mutations while holding pending lock and
    // increment in_flight before releasing it, so wait_until_quiet has no gap.
    // One spawn_blocking worker serializes same-key application. Rejected
    // cross-instance writes mark workspace incomplete for reconciliation.
}
```

```rust
// src/session_store.rs / src/session_core.rs
pub fn save_acp_result(...) -> io::Result<PersistedWrite> {
    // Serialize record, write to per-write UUID temp path, atomic rename,
    // stat actual post-rename mtime, return bounded summary only on success.
}

// SessionPersistence has optional daemon-only SessionCatalogWriter.
// Successful save enqueues saved metadata; failed write logs and never indexes.
// Successful/idempotent first delete enqueues a materialized tombstone. Repeated
// delete returns false without refreshing tombstone retention.
```

```rust
// src/session_store.rs
pub fn scan_summaries(max_preview, after_name, max_entries, deadline) -> SessionSummaryScan {
    // Walk until deadline, retain only lexicographically smallest max_entries+1
    // names above stable cursor in a bounded BTreeMap, parse at most max_entries
    // payloads, return sorted summaries + next filename cursor + complete flag.
}
```

```rust
// src/session_daemon.rs
struct CatalogDiscovery {
    catalog: SessionCatalog,
    writer: Arc<SessionCatalogWriter>,
    workspace_id: String,
    reconcile_entries: usize,
    reconcile_owner: String,
    reconcile_interval: Duration,
    full_rescan_interval: Duration,
    last_full_rescan_unix_ms: AtomicU64,
    tombstone_retention: Duration,
}

async fn run_catalog_blocking(&self, work) -> Result<R> {
    // Acquire OwnedSemaphorePermit with timeout; move permit into spawn_blocking;
    // outer timeout may cancel await but blocking job retains permit until exit.
}

async fn durable_listing_rows(&self) -> (Vec<SessionListEntry>, bool) {
    // Healthy+complete catalog: bounded indexed query.
    // Periodically mark complete catalog incomplete to start a full drift sweep.
    // Cold/incomplete/newer-schema/unhealthy: one bounded factory scan and
    // return incomplete=true rather than silently claiming completeness.
}

async fn reconcile_catalog_once(&self, discovery) -> Result<()> {
    // Acquire global DB lease. workspace_state cursor phases:
    // dir:<filename>: bounded stable payload batch -> authoritative upserts.
    // ghost:<session_id>: bounded catalog-id batch -> validate JSON, materialize
    // missing/corrupt ghosts as authoritative tombstones. On completion mark
    // workspace complete and purge one bounded old-tombstone batch.
}
```

```rust
// Task-1337 integration retained and extended.
struct ConnectionListingSnapshot {
    rows: Vec<SessionListEntry>,
    offset: usize,
    expected_cursor: Option<String>,
    expires_at: Instant,
    incomplete: bool,
    valid: Arc<AtomicBool>,
}

// Global bounded registry stores Weak validity flags. New snapshots prune dead
// connections and invalidate oldest live cursors at capacity. Connection drop
// drops the rows. list_sessions_for_connection freezes (rows,incomplete), and
// every page passes snapshot.incomplete to fit_session_list_to_frame.

// ConnectionTrust remains separate from capabilities: LocalOwner receives rich
// workspace/model/time/preview/count/status; RemotePaired receives only the
// pre-existing session_id/active/attached count. Random rotating cursors remain
// physical-connection-local; cross-trust/connection replay is invalid_cursor.
```

```rust
// Protocol/client/mirror changes
ServerMessage::SessionList adds backward-compatible defaulted/omitted
`incomplete: bool`. SessionClientOutcome and TuiSessionList preserve it through
SessionClient -> SessionController -> TuiSession. Android SessionList adds
`incomplete: Boolean = false`; fixture tests true and default false decoding.
```

```rust
// Startup wiring
SessionCatalog::open beside daemon JSON. Newer/failed catalog => bounded fallback.
Ready catalog is marked incomplete at daemon start, writer injected only into
AgentSessionFactory/SessionPersistence, and SessionDaemon receives catalog,
writer, workspace identity, reconcile/query/fallback/snapshot limits. Shutdown
waits for writer quiet within existing shutdown grace.
```

Config/default docs add: optional catalog path; busy timeout; pending entries;
write batch; query timeout (> busy timeout); query concurrency; fallback entries;
reconcile entries; shared reconcile interval; full rescan interval; tombstone
retention; global listing snapshot capacity. ADR-010 records JSON authority,
trust boundary, incomplete pages, cursor replacement, and catalog lifecycle.

Tests in actual diff cover: WAL multiprocess visibility; newer schema untouched;
older schema rebuild without payload mutation; same/cross writer ordering;
equal-mtime tombstone and authoritative recreate; stale resurrection prevention;
writer coalescing/saturation/incomplete state; failed write exclusion; unique
concurrent temp paths; writer task lifecycle; shutdown drain primitive; global
reconcile lease; stable bounded scan/deadline; directory then ghost reconciliation;
catalog-ready listing; incomplete every page; query permit retained after timeout;
global snapshot oldest eviction; and all task-1337 trust/cursor/frame/metadata
regressions.

Verification before review: cargo fmt/clippy clean; cargo test 1751 passed/10 ignored; pytest 296 passed. Android Gradle remains locally blocked by absent Java, but the Kotlin test is included.

### Known Concerns
1. Reconciliation is list-triggered rather than a permanent background timer; after completeness, the first list past full_rescan_interval starts another bounded sweep and returns incomplete fallback.
2. Equal-mtime cross-instance live writes are conservatively rejected; authoritative reconciliation may supersede them and marks the workspace incomplete when a writer mutation is rejected.
3. Fallback is deterministic and bounded but cannot certify global newest-first ordering, so every frozen fallback page carries incomplete=true.
4. Task 1337 components were modified to propagate incomplete and add global snapshot eviction; they require explicit regression review.
5. Android test cannot run locally because Java/JAVA_HOME is absent.

### Specific Questions for Reviewer
1. Audit mutation ordering across two daemon processes, restart, save/delete/recreate, writer saturation, and shutdown. Can a stale upsert resurrect a tombstoned or missing payload?
2. Audit reconciliation cursor progress, global lease/rate bound, corrupt/foreign payloads, ghost detection, equal-mtime repair, and completeness transitions.
3. Audit cancellation and resource lifecycle: timed-out blocking jobs, semaphore permits, writer task ownership, pending-map bounds, snapshot drop/eviction, and SQLite newer-schema handling.
4. Explicitly re-review task 1337: LocalOwner/RemotePaired privacy, rich row semantics, ordering, frame trimming, connection/trust cursor binding, incomplete propagation on every page, ServerError behavior, TUI storage, and Android compatibility.
5. Identify any missing config/docs/tests or behavior that falsely claims completeness.

<!-- event id="review_response_1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Cross-writer conflict resolution relies on 'newer actual payload mtime' to accept a different writer's upsert, but filesystem mtime granularity (commonly 1s on some filesystems, and coarse on network/overlay filesystems) plus clock skew between daemon processes means two distinct writes can carry equal or inverted mtimes. The equal-mtime tombstone-wins rule covers delete-vs-live, but a live-vs-live equal-mtime cross-instance write is rejected and only repaired via reconciliation marking the workspace incomplete. This makes correctness depend on reconciliation always running to completion. Please demonstrate (with a test) that a rejected cross-instance live write cannot remain silently absent from listings while the workspace is later marked complete — i.e., prove the rejected-write -> mark-incomplete -> reconcile -> complete chain has no path where mark-incomplete is lost (e.g., writer marked unhealthy, process crash between apply-rejection and workspace_state update, or the incomplete flag being set in a different transaction than the rejected apply).
B2. Tombstone resurrection window across restart: generation is an AtomicU64 that presumably restarts at 0 after daemon restart while writer_instance_id changes per instance. After restart, the same-writer 'increasing generation' branch never applies (new instance id), so acceptance falls to the mtime comparison. Sequence: instance A tombstones session S (materialized tombstone with observed mtime T1); user recreates S with payload mtime T2 > T1 via instance B; B crashes after rename but before enqueue/apply; instance C's stale in-flight save of the OLD pre-delete payload (queued before delete, coalesced map already drained) could apply with mtime > tombstone's recorded mtime. The diff claims a 'stale resurrection prevention' test exists — please confirm it covers the restart + cross-instance + coalescing-drain interleaving specifically, not just single-process generation monotonicity, and cite the test name(s). If not covered, add the test or a design argument for why the interleaving is impossible.
B3. Timed-out blocking work retains the query permit until the blocking job exits (correct for safety), but with query concurrency N and a wedged/slow SQLite operation (e.g., another process holding a long write lock beyond busy_timeout on a filesystem where busy handler polling degrades), N consecutive timeouts exhaust all permits and every subsequent list request fails permit acquisition until blocked jobs drain. There is no visible circuit breaker or health transition tied to repeated permit-acquisition timeouts. Specify the observable behavior in this state (error surfaced to client? fallback scan without catalog? silent empty list?) and ensure the fallback path does not itself require a permit. If fallback scan is filesystem-only and permit-free, document and test that permit exhaustion degrades to bounded fallback with incomplete=true rather than to request failure.
B4. Reconciliation lease is a single global row in the shared catalog with owner/expiry replaced only when expired. If the lease holder is a daemon that dies without expiry cleanup, reconciliation is blocked until expiry — acceptable — but the lease acquisition uses BEGIN IMMEDIATE against a DB that other processes write with WAL/NORMAL. With synchronous=NORMAL, a power loss can lose the last WAL frames: a lease acquisition that was observed by the acquirer may be rolled back after crash recovery, allowing a second daemon to acquire an 'expired' lease while the first (which survived, e.g., only the machine's WAL sync was lost — or more realistically after a checkpoint anomaly) believes it still holds it. Two concurrent reconcilers writing authoritative_observation upserts and tombstone purges could interleave. Either (a) argue durability loss implies the first holder also crashed (single-machine assumption) and document that assumption in ADR-010, or (b) make reconciliation writes themselves lease-guarded (re-verify owner in the same transaction as each authoritative batch). Currently the diff shows lease checks only at acquisition, not per-batch.
B5. durable_listing_rows 'periodically marks complete catalog incomplete to start a full drift sweep' — this means at every full_rescan_interval boundary, listings degrade to bounded fallback scans with incomplete=true even when nothing changed, and completeness only returns after a full reconciliation pass. On large session directories this makes the UI periodically flip to incomplete pages. Known-concern #1 acknowledges this, but the request does not state what bounds the incomplete window: reconciliation is list-triggered, so a workspace that is listed exactly once at the interval boundary starts a sweep, returns incomplete, and if no further list arrives, the sweep may never progress (cursor phases advance only when reconcile_catalog_once runs). Confirm reconcile_catalog_once is driven to completion independent of further list calls (e.g., self-rescheduling until complete), or accept and document that completeness is only restored on subsequent list activity — and add a test for 'single list at rescan boundary eventually restores completeness' or explicitly test the documented limitation.
B6. Task-1337 regression surface: the global bounded snapshot registry 'invalidates oldest live cursors at capacity.' This introduces a new cross-connection denial vector — a LocalOwner (or any client able to open many connections/listings) can churn snapshots and invalidate other connections' in-flight pagination, converting their next page request into invalid_cursor. Previously cursors were purely connection-local with connection-local lifecycle. Confirm: (1) invalid_cursor on eviction is surfaced identically to the pre-existing expiry path (client re-lists cleanly, no error leak of other connections' existence), (2) RemotePaired connections cannot observe capacity effects that reveal LocalOwner activity (timing/eviction side channel is acceptable to document as out of scope, but state it), and (3) capacity is per-trust-domain or at least large enough that the eviction test covers a RemotePaired victim of LocalOwner-driven eviction.

### Non-Blocking Suggestions
S1. Writer saturation sets mark_incomplete on distinct-key overflow but silently drops the new mutation. Consider logging at warn with the session_id count (rate-limited) so operators can distinguish 'incomplete due to saturation' from 'incomplete due to reconcile sweep' when tuning max_pending.
S2. Config requires query timeout > busy timeout by documentation only. Validate this at startup and fail fast (or clamp with a warning) rather than allowing a configuration where the busy handler consumes the entire query timeout and every permit is retained by a job that will always overrun.
S3. Older-schema handling drops/rebuilds catalog tables under BEGIN IMMEDIATE, but during rebuild other processes running the same (old) binary version may hold Ready connections against the old schema. Consider bumping user_version/schema_meta first within the same transaction and having apply/rows tolerate 'table changed underfoot' as a transient error that flips the workspace to incomplete, and add a mixed-version two-process test if not already present.
S4. Android: since Gradle cannot run locally, add the Kotlin fixture test to CI required checks in this change (or note the follow-up task id) so the incomplete-field default-decoding coverage is actually executed somewhere before release.
S5. ADR-010 should explicitly record the single-machine assumption for the SQLite catalog (WAL requires a common local filesystem; catalog on NFS is unsupported) since several correctness arguments (mtime comparison, lease, WAL visibility) depend on it.

**Verdict:** REVISE

<!-- event id="rebuttal_1" artifact path="1336-bounded-session-discovery-implementation/artifacts/round-1-rebuttal.diff" sha256="8fc2633e361788d378ce201f79981dbd9f71ea7323040c1fde010fec17e8af5a" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — Added an end-to-end rejected-write repair test: a complete workspace receives an equal-mtime cross-instance live update, the writer rejects it and marks workspace_state incomplete, authoritative reconciliation supersedes it, then and only then completeness is restored with the new model visible. If the process crashes between the rejected apply and separate incomplete update, every daemon startup already calls catalog.mark_incomplete before constructing the writer, so the next process cannot serve that workspace as complete. If the DB update errors without process death, writer.healthy becomes false and listing bypasses catalog for incomplete fallback.
Re B2: DISPUTE — The proposed stale resurrection ordering is causally blocked on the supported single-host local filesystem. A mutation is created only after its payload rename and actual post-rename mtime are observed. If that old rename precedes delete, the later tombstone timestamp is greater or equal; different-writer older live data loses, and equal-time live data loses to deleted=1. Added `restarted_writer_cannot_resurrect_newer_tombstone` for the exact changed-writer/low-generation path. Same-writer ordering remains generation-guarded. A recreate that is currently authoritative is repaired by authoritative reconciliation (`authoritative_reconciliation_can_restore_equal_time_recreate`). Clock rollback can temporarily invert wall time, but startup/full reconciliation marks incomplete and JSON remains authoritative; that residual is documented under the local-filesystem model.
Re B3: DISPUTE — Permit exhaustion already has explicit tested behavior and a permit-free fallback would violate the accepted bound. `timed_out_fallback_keeps_permit_until_blocking_scan_finishes` runs concurrency=1, times out a blocking scan, then proves a second request cannot start another scan (`max_active == 1`) and receives `(empty, incomplete=true)`, not a request failure or false completeness. Empty is not silent because `incomplete` propagates on every snapshot page. Query timeout is validated greater than SQLite busy timeout. Once the blocking task exits, its owned permit is released.
Re B4: ACCEPT — Documented the supported single-host local-filesystem assumption explicitly; WAL catalogs on NFS/network filesystems are unsupported. Also documented that the global lease is rate limiting, not the correctness lock. Mutation guards, authoritative reconciliation, and materialized tombstones are idempotent if reconcilers overlap, so correctness does not depend on exclusive lease durability.
Re B5: ACCEPT — The design intentionally remains list-triggered rather than adding a permanent timer. Documentation now states that one request may start a sweep and leave the workspace explicitly incomplete until subsequent list requests advance it. `reconciliation_progresses_directory_then_removes_catalog_ghosts` now asserts the first request leaves workspace_state incomplete before later bounded passes restore completeness. This is a visible, tested limitation, not false completeness.
Re B6: ACCEPT — Partitioned the global bounded snapshot registry by ConnectionTrust. LocalOwner churn can evict only local cursors; RemotePaired churn can evict only remote cursors. Added `local_snapshot_churn_cannot_evict_remote_cursor`, while the existing capacity test verifies same-domain oldest eviction becomes ordinary invalid_cursor. ADR-010 documents same-domain timing/eviction as outside the cross-trust privacy guarantee. No error reveals which other connection caused eviction.

### Revised Code / Diff
```diff
--- a/src/session_daemon.rs
+++ b/src/session_daemon.rs
@@
-    listing_snapshot_registry: Mutex<VecDeque<Weak<AtomicBool>>>,
+    local_listing_snapshot_registry: Mutex<VecDeque<Weak<AtomicBool>>>,
+    remote_listing_snapshot_registry: Mutex<VecDeque<Weak<AtomicBool>>>,
@@
-    fn register_listing_snapshot(&self) -> Arc<AtomicBool> {
-        let mut registry = self.listing_snapshot_registry.lock()...;
+    fn register_listing_snapshot(&self, trust: ConnectionTrust) -> Arc<AtomicBool> {
+        let registry = match trust {
+            ConnectionTrust::LocalOwner => &self.local_listing_snapshot_registry,
+            ConnectionTrust::RemotePaired => &self.remote_listing_snapshot_registry,
+        };
+        let mut registry = registry.lock()...;
         registry.retain(|entry| entry.strong_count() > 0);
         while registry.len() >= self.listing_snapshot_global_capacity {
             if let Some(evicted) = registry.pop_front().and_then(|entry| entry.upgrade()) {
                 evicted.store(false, Ordering::Release);
             }
         }
         registry.push_back(Arc::downgrade(&valid));
         valid
     }
@@
-    valid: self.register_listing_snapshot(),
+    valid: self.register_listing_snapshot(trust),
```

```rust
#[tokio::test]
async fn local_snapshot_churn_cannot_evict_remote_cursor() {
    // global capacity 1 per trust domain; mint RemotePaired cursor,
    // create a LocalOwner snapshot, then successfully consume remote cursor.
}

#[tokio::test]
async fn rejected_cross_instance_write_repairs_before_completeness_returns() {
    // Start complete with writer A row; writer B equal-mtime live mutation is
    // rejected and marks state incomplete. Authoritative reconciliation applies
    // current payload metadata; set complete; assert new model and complete=true.
}

#[test]
fn restarted_writer_cannot_resurrect_newer_tombstone() {
    // writer_instance changes and generation restarts, but older actual payload
    // mtime cannot supersede newer materialized tombstone.
}
```

```diff
--- a/src/session_catalog.rs
+++ b/src/session_catalog.rs
@@
         if distinct_key_capacity_exceeded {
+            if !pending.mark_incomplete {
+                tracing::warn!(event = "catalog_pending_saturated", pending, max_pending);
+            }
             pending.mark_incomplete = true;
         }
```

```markdown
ADR-010 additions:
- Catalog supports one host's local filesystem only; WAL on NFS/network FS unsupported.
- Lease rate-limits reconciliation but is not a correctness lock; guarded authoritative
  mutations and tombstones tolerate overlap.
- Reconciliation is list-triggered; one request can start a sweep and remain explicitly
  incomplete until subsequent list requests advance it.
- Snapshot capacity is partitioned by trust; same-domain eviction maps to invalid_cursor,
  while LocalOwner churn cannot evict RemotePaired cursors.
```

Existing tests cited for B3: timed_out_fallback_keeps_permit_until_blocking_scan_finishes. Existing config validation rejects zero catalog limits and query_timeout <= busy_timeout. Focused tests and Clippy pass.

### New Points
R1. Completeness is the safety signal: writer rejection, pending saturation, startup, catalog errors, and periodic sweeps all force incomplete/fallback before reconciliation may restore complete.
R2. The reconciliation lease deliberately bounds aggregate work but is not used for mutual-exclusion correctness; SQL mutation guards and materialized tombstones remain valid under overlap.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. The B2 dispute is accepted: on a single-host local filesystem, the causal argument (mutation created only after observing actual post-rename mtime, so a pre-delete rename cannot carry a timestamp exceeding the later tombstone's) plus restarted_writer_cannot_resurrect_newer_tombstone and the equal-mtime deleted=1-wins rule closes the interleaving I described. Suggestion only: add a one-line code comment at the mtime-comparison branch in apply() referencing this causal argument and the clock-rollback residual, so a future refactor that decouples mutation creation from post-rename stat does not silently reopen the resurrection window.
S7. The B3 dispute is accepted: timed_out_fallback_keeps_permit_until_blocking_scan_finishes demonstrates that permit exhaustion degrades to (empty, incomplete=true) rather than request failure, and incomplete propagation on every page makes it non-silent. Suggestion only: consider a rate-limited warn log or counter when a list request returns fallback specifically due to permit-acquisition timeout (as distinct from cold/incomplete catalog), so operators can diagnose a wedged SQLite writer without correlating client-side incomplete flags.
S8. Carry-forward of prior non-blocking items not addressed in the revision: S3 (mixed-version two-process schema rebuild tolerance) and S4 (ensuring the Android Kotlin fixture test runs in CI since Gradle is locally blocked) remain open as suggestions. Neither blocks approval, but S4 in particular should get a follow-up task id so the incomplete-field default-decoding coverage executes somewhere before release.

### Responses to Prior Points
Re R1: Accepted. The invariant 'any correctness gap forces incomplete before completeness may be restored' is the right safety framing, and the revised diff now backs it at each entry point: startup mark_incomplete precedes writer construction (covering crash between rejected apply and workspace_state update), writer.healthy=false on DB error bypasses the catalog entirely, saturation forces incomplete with a rate-limited warn, and the periodic sweep is explicitly incomplete until reconciliation finishes. With rejected_cross_instance_write_repairs_before_completeness_returns covering the end-to-end chain, I consider the completeness-as-safety-signal argument demonstrated rather than merely asserted.
Re R2: Accepted. Recasting the lease as a rate limiter rather than a mutual-exclusion primitive resolves my durability objection: if reconciler overlap is tolerated by construction (guarded SQL upserts, idempotent materialized tombstones, monotone completeness transitions), then WAL/NORMAL frame loss on the lease row cannot corrupt state, only permit redundant bounded work. The ADR-010 documentation of this distinction plus the single-host local-filesystem assumption closes the concern. I did not find a reconciliation write in the excerpts whose correctness silently depends on exclusivity.
B1: resolved
B2: resolved
B3: resolved
B4: resolved
B5: resolved
B6: resolved

**Verdict:** APPROVE
