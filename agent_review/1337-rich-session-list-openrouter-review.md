# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1337-openrouter-verification-20260827" -->

<!-- event id="request" artifact path="1337-rich-session-list-openrouter-review/artifacts/round-1-review-request.diff" sha256="ad48e268ce4a639494a464acde4251563b67bc18509eef3984c03fcb45521423" -->
## Review Request — Round 1
**Task:** 1337 — Automated verification of rich daemon session-list implementation
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Expose rich daemon session metadata safely and end-to-end. LocalOwner connections receive canonical workspace identity, model, timestamp, bounded preview, provider-history message count when available, active state, attachment count, and optional live turn status. RemotePaired connections retain minimal rows. Durable scans run in spawn_blocking as an interim before task 1336's SQLite catalog. Pages come from bounded expiring connection-local snapshots with rotating opaque cursors and typed invalid_cursor errors. Typed rows propagate through SessionClient, SessionController, TuiSession, and the additive Android mirror.

### Relevant Code / Diff
Review the current uncommitted workspace diff for task 1337. Load-bearing actual code is below.

```rust
// src/session_protocol.rs
pub struct SessionListEntry {
    pub session_id: String,
    pub active: bool,
    pub attached_clients: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_status: Option<TurnStatus>,
}

pub struct SessionWorkspace { pub id: String, pub label: String }

// ServerMessage::SessionList adds:
workspace: Option<SessionWorkspace>
// ProtocolLimits adds max_cursor_bytes; ListSessions validates against it.
```

```rust
// src/session_daemon.rs
pub enum ConnectionTrust { LocalOwner, RemotePaired }

struct ConnectionListingSnapshot {
    rows: Vec<SessionListEntry>,
    offset: usize,
    expected_cursor: Option<String>,
    expires_at: tokio::time::Instant,
}

async fn listing_rows(&self) -> Result<Vec<SessionListEntry>, SessionListPageError> {
    let mut sessions = BTreeMap::new();
    if let Some(factory) = self.factory.as_ref() {
        for summary in factory.persisted_session_summaries(self.session_list_preview_bytes).await {
            sessions.insert(summary.id.clone(), SessionListEntry {
                session_id: summary.id,
                active: false,
                attached_clients: 0,
                model: Some(summary.model),
                updated_at_unix_ms: summary.updated_at.map(system_time_unix_ms),
                preview: summary.first_user_line,
                message_count: Some(summary.message_count),
                turn_status: None,
            });
        }
    }
    let active: Vec<_> = self.sessions.lock().unwrap_or_else(|p| p.into_inner())
        .iter().map(|(id, entry)| (id.clone(), Arc::clone(entry))).collect();
    for (session_id, entry) in active {
        let attached_clients = entry.clients.lock().unwrap_or_else(|p| p.into_inner()).len();
        let (turn_status, snapshot_preview) = {
            let snapshot = entry.snapshot.lock().unwrap_or_else(|p| p.into_inner());
            let preview = snapshot.snapshot.transcript.iter()
                .find(|entry| entry.role == TranscriptRole::User)
                .and_then(|entry| entry.text.lines().next())
                .and_then(|line| session_store::normalize_preview(line, self.session_list_preview_bytes));
            (snapshot.snapshot.turn_status, preview)
        };
        let (message_count, preview) = match entry.core.session.try_lock() {
            Ok(session) => (
                Some(session.history().len()),
                session_store::first_user_preview(session.history(), self.session_list_preview_bytes),
            ),
            Err(_) => (None, snapshot_preview),
        };
        sessions.insert(session_id.clone(), SessionListEntry {
            session_id,
            active: true,
            attached_clients,
            model: Some(entry.core.current_model()),
            updated_at_unix_ms: Some(entry.last_activity_unix_ms.load(Ordering::Acquire)),
            preview,
            message_count,
            turn_status: Some(turn_status),
        });
    }
    let mut rows: Vec<_> = sessions.into_values().collect();
    rows.sort_by(|a,b| b.updated_at_unix_ms.unwrap_or_default()
        .cmp(&a.updated_at_unix_ms.unwrap_or_default())
        .then_with(|| a.session_id.cmp(&b.session_id)));
    if rows.len() > self.session_list_snapshot_entries {
        return Err(SessionListPageError::CapacityExceeded);
    }
    Ok(rows)
}

async fn list_sessions_for_connection(
    &self,
    request_id: String,
    cursor: Option<&str>,
    trust: ConnectionTrust,
    snapshot: &mut Option<ConnectionListingSnapshot>,
    max_frame_bytes: usize,
) -> Result<ServerMessage, SessionListPageError> {
    if cursor.is_none() {
        *snapshot = Some(ConnectionListingSnapshot {
            rows: self.listing_rows().await?,
            offset: 0,
            expected_cursor: None,
            expires_at: Instant::now() + self.session_list_snapshot_ttl,
        });
    }
    let listing = snapshot.as_mut().ok_or(SessionListPageError::InvalidCursor)?;
    if Instant::now() >= listing.expires_at
        || cursor.is_some_and(|cursor| listing.expected_cursor.as_deref() != Some(cursor))
    {
        *snapshot = None;
        return Err(SessionListPageError::InvalidCursor);
    }
    let end = listing.offset.saturating_add(self.session_list_page_size).min(listing.rows.len());
    let rows = render_listing_rows(&listing.rows[listing.offset..end], trust);
    let remaining = listing.rows.len().saturating_sub(listing.offset);
    let next_cursor = format!("v1_{}", Uuid::new_v4());
    let workspace = (trust == ConnectionTrust::LocalOwner)
        .then(|| self.workspace_identity.clone()).flatten();
    let (message, sent) = fit_session_list_to_frame(
        request_id, workspace, rows, remaining, next_cursor, max_frame_bytes,
    ).map_err(SessionListPageError::TooLarge)?;
    listing.offset = listing.offset.saturating_add(sent);
    listing.expected_cursor = match &message {
        ServerMessage::SessionList { next_cursor, .. } => next_cursor.clone(),
        _ => unreachable!(),
    };
    if listing.expected_cursor.is_none() { *snapshot = None; }
    Ok(message)
}

fn render_listing_rows(rows: &[SessionListEntry], trust: ConnectionTrust) -> Vec<SessionListEntry> {
    rows.iter().cloned().map(|mut row| {
        if trust == ConnectionTrust::RemotePaired {
            row.model = None;
            row.updated_at_unix_ms = None;
            row.preview = None;
            row.message_count = None;
            row.turn_status = None;
        }
        row
    }).collect()
}

fn fit_session_list_to_frame(
    request_id: String,
    workspace: Option<SessionWorkspace>,
    mut sessions: Vec<SessionListEntry>,
    remaining: usize,
    cursor: String,
    max_frame_bytes: usize,
) -> Result<(ServerMessage, usize), String> {
    loop {
        let next_cursor = (sessions.len() < remaining).then(|| cursor.clone());
        let message = ServerMessage::SessionList {
            request_id: request_id.clone(),
            workspace: workspace.clone(),
            sessions: sessions.clone(),
            next_cursor,
        };
        if serde_json::to_vec(&message).map_err(|e| e.to_string())?.len() <= max_frame_bytes {
            return Ok((message, sessions.len()));
        }
        sessions.pop();
        if sessions.is_empty() { return Err("one entry exceeds frame".to_string()); }
    }
}
```

```rust
// Connection trust is explicit in CapabilityPolicy. serve_client uses
// LocalOwner; serve_client_with_capabilities (remote gateway) uses RemotePaired.
// One listing_snapshot variable lives inside each serve_client_with_policy loop.
// Unknown, replayed, expired, cross-connection and oversized cursors return a
// nonfatal ServerMessage::Error with code invalid_cursor. Capacity excess and
// frame excess have separate typed string codes.
```

```rust
// src/session_factory.rs
// workspace is canonicalized at construction. SessionWorkspace.id is SHA-256
// over canonical path, while label is a UTF-8 bounded basename. Legacy records
// canonicalize stored cwd before matching. Durable scan is isolated:
async fn persisted_session_summaries(&self, max_preview_bytes: usize) -> Vec<SessionSummary> {
    let store = self.store.clone();
    let workspace = self.workspace.clone();
    tokio::task::spawn_blocking(move || {
        store.list_with_preview_limit(max_preview_bytes).into_iter()
            .filter(|summary| summary.cwd.as_deref()
                .and_then(|cwd| std::fs::canonicalize(cwd).ok())
                .is_some_and(|cwd| cwd == workspace))
            .collect()
    }).await.unwrap_or_default()
}
```

```rust
// src/session_store.rs
// first_user_preview selects first user text line. normalize_preview incrementally
// collapses whitespace and uses floor_char_boundary so output never exceeds the
// configured byte cap and never splits UTF-8.
```

```rust
// src/session_client.rs
pub enum SessionClientOutcome {
    SessionList {
        request_id: String,
        workspace: Option<SessionWorkspace>,
        sessions: Vec<SessionListEntry>,
        next_cursor: Option<String>,
    },
    ServerError { request_id: Option<String>, code: String, message: String },
    // ...
}
// Attached-session ServerMessage::Error maps to ServerError (nonterminal);
// attach-handshake Error remains SessionClientError::Server.
```

```rust
// SessionControllerCommand::ListSessions forwards through SessionClient and
// blocks session switching while in flight. TuiSession keeps bounded pending
// operation/request maps, stores typed rows, exposes take_session_list(), and
// clears list state on a correlated ServerError.
```

```kotlin
// Android WireModels adds nullable workspace and nullable rich row fields.
// ProtocolFixtureTest decodes both rich local and minimal remote rows.
```

The full diff also adds configurable preview/cursor/snapshot-entry/snapshot-TTL limits in config, default TOML, and docs; updates ADR-010; and adds extensive Rust tests for trust, cursor lifecycle, ordering, frame fitting, capacity, active/busy metadata, legacy workspace aliases, client/controller/TUI propagation, and nonfatal errors.

### Known Concerns
1. Task 1336 still owns replacing the interim full flat-file scan with a bounded multiprocess SQLite catalog.
2. Android Gradle tests are present but could not run locally because Java/JAVA_HOME is unavailable.
3. For a busy active session, message_count is deliberately None while preview comes from canonical snapshot state rather than presenting stale durable data.
4. The snapshot capacity returns session_list_capacity rather than silently omitting sessions.

### Specific Questions for Reviewer
1. Find any actionable correctness, privacy, pagination, async blocking, compatibility, or lifecycle defects in the actual current diff.
2. Verify that remote clients cannot recover rich local metadata by cursor replay or capability selection.
3. Verify frame trimming cannot skip or duplicate snapshot rows.
4. Verify attached-session ServerError handling does not trigger inappropriate reconnects or leak pending operations.

<!-- event id="review_response_1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Pagination can duplicate or skip rows relative to what the client believes it received because `fit_session_list_to_frame` computes `next_cursor` from `sessions.len() < remaining` BEFORE trimming. Concretely: `remaining` is rows left including the current page. If the initial page slice length equals `remaining` (last page), `next_cursor` starts as None; when trimming then pops entries to fit the frame, subsequent loop iterations recompute `(sessions.len() < remaining)` which becomes true, so the cursor is correctly re-added in that direction. However, the inverse hazard is real: `listing.offset` is advanced by `sent` (the trimmed count) but `listing.expected_cursor` is taken from the message; if trimming pops rows down to a page where `sessions.len() < remaining` is still false only when sessions.len()==remaining==0 edge, an empty final page can be emitted with `next_cursor: Some(...)` never cleared — specifically when every row is popped is handled by an error, but when `remaining > 0` and the page slice was empty (offset == rows.len() after a prior exact-fit page), the client receives `sessions: []` with `next_cursor` computed from `0 < remaining`. Verify and add tests for: (a) exact page boundary where offset reaches rows.len() with remaining==0 — currently `remaining = rows.len() - offset` is 0, `0 < 0` is false, OK; (b) trimmed-final-page where trimming converts a terminal page into a continued page — the offset advance uses `sent`, which is correct, but there is no test cited proving no row is skipped or resent after trimming across the trim boundary. At minimum, add a property/unit test that walks a full multi-page listing with an artificially small max_frame_bytes and asserts the concatenation of pages equals the snapshot rows exactly once, in order. Without that test the load-bearing invariant asked about in question 3 is unverified.
B2. Cursor validation on the FIRST page is asymmetric and permits a stale-snapshot read: in `list_sessions_for_connection`, when `cursor.is_some()` but `snapshot` already exists and `listing.expected_cursor` is None (i.e., the previous listing completed and was supposed to be cleared — it is cleared, good), the None-snapshot path returns InvalidCursor, fine. But when `cursor.is_none()`, the code unconditionally rebuilds the snapshot without checking TTL of the old one — acceptable — yet the expiry check `Instant::now() >= listing.expires_at` is evaluated AFTER snapshot creation on the fresh path, meaning a `session_list_snapshot_ttl` of zero (misconfiguration) makes every first-page request fail with invalid_cursor rather than a config error. More importantly: the expiry branch on the fresh-snapshot path can never legitimately fire, so the check silently conflates two states; and on the cursor path, a request with a VALID cursor arriving exactly at expiry clears the snapshot and returns invalid_cursor, which is fine, but nothing distinguishes 'expired' from 'replayed/foreign' in the error surface for clients that want to silently restart vs. warn. Require: (1) validate/reject ttl==0 at config load, and (2) either document that invalid_cursor covers expiry or emit a distinct retryable code, and add a test for a cursor presented after TTL expiry.
B3. Potential blocking/lock-ordering hazard in `listing_rows`: it holds no locks across await points (good — the `sessions` mutex is copied out first), but `entry.core.session.try_lock()` falling back to snapshot data means `message_count: None` for busy sessions while `preview` may come from a snapshot that reflects an OLDER transcript than `updated_at_unix_ms` (loaded from `last_activity_unix_ms` with Acquire). That is acknowledged in known_concerns for message_count, but the same acknowledgment does not cover the preview: `snapshot.snapshot.transcript` first-user-line could differ from `session.history()` first-user-line if the first user message was edited/truncated, producing rows whose preview flips between consecutive pages of the SAME snapshot? No — rows are frozen in the snapshot, so within one listing it is stable. The residual issue is cross-listing inconsistency only, which is acceptable. Downgrade candidate — however, one genuine defect remains here: `factory.persisted_session_summaries` is awaited while building rows, and its `spawn_blocking` closure calls `std::fs::canonicalize` PER RECORD on the blocking pool; with a large legacy store this makes first-page latency unbounded and, because the durable scan runs on every fresh (cursor-less) list request per connection with no daemon-level cache or coalescing, N clients refreshing concurrently multiply full-store filesystem scans. Task 1336 covers the SQLite replacement, but the interim needs at least a short daemon-level cache or single-flight coalescing to avoid trivially DoS-able disk scans triggered by the RemotePaired gateway path too (remote clients still trigger the full durable scan even though all rich fields are then stripped — they pay to compute data they cannot see). Minimal fix: skip `persisted_session_summaries` entirely for RemotePaired connections, since every durable-only field is redacted anyway and durable-only rows would render as empty minimal rows.
B4. Privacy check (question 2) is close but has one gap: `render_listing_rows` strips rich fields for RemotePaired, and workspace is gated on LocalOwner — good. But durable-only (inactive) rows still reach RemotePaired clients as `{session_id, active:false, attached_clients:0}`. Session IDs of never-attached, purely-local historical sessions are themselves metadata leakage to remote peers (existence + count of local sessions in the workspace). Confirm whether the product intent is that RemotePaired sees only sessions it could attach to; if so, filter durable-only rows out of the RemotePaired listing (this also resolves the wasted scan in B3). Additionally verify there is a test asserting a cursor minted on a LocalOwner connection presented on a RemotePaired connection returns invalid_cursor — the description says snapshots are connection-local (one variable per serve loop), which structurally guarantees it, but the claimed test list says 'cursor lifecycle' generically; require an explicit cross-trust replay test.

### Non-Blocking Suggestions
S1. In `listing_rows`, active-session insertion overwrites durable rows keyed by session id via BTreeMap::insert — correct precedence — but sorting uses `updated_at_unix_ms.unwrap_or_default()`, which sinks all RemotePaired... no, sorting happens pre-redaction, fine; however durable rows with missing updated_at sort to the bottom interleaved only by id. Consider a stable secondary bucket (active-first) so a busy active session whose last_activity is stale doesn't get buried below cold durable rows.
S2. `fit_session_list_to_frame` serializes the entire message once per popped row (O(n^2) serialization worst case with large pages). Consider binary-searching the fitting count or estimating per-row serialized size to bound work.
S3. The rotating cursor `v1_{uuid}` is minted even when the page is terminal and then discarded; mint it lazily only when `next_cursor` will be Some to avoid implying continuation in logs/traces.
S4. CI cannot currently attest the Android Gradle tests (no JAVA_HOME). Before merge, run ProtocolFixtureTest in an environment with Java, or gate the Android mirror change behind a CI job; a schema-mirroring change verified only by unexecuted tests is a compatibility risk given the additive nullable-field claim.
S5. Document (in ADR-010) that within a single connection, issuing a fresh cursor-less ListSessions while a paginated listing is in flight silently discards the old snapshot; clients interleaving refresh and next-page will get invalid_cursor on the stale cursor. That is correct behavior but worth stating as protocol contract.

**Verdict:** REVISE

<!-- event id="rebuttal_1" artifact path="1337-rich-session-list-openrouter-review/artifacts/round-1-rebuttal.diff" sha256="c73686039c7c9b590d1e6400bad44a133d69469f013430bc39e542ac3653aef2" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — The algorithm already advances by the actual trimmed `sent` count and recomputes continuation after every pop, but the complete no-skip/no-duplicate invariant lacked direct coverage. Added `frame_trimmed_pages_cover_snapshot_rows_once_in_order`: it walks a 12-row frozen snapshot through repeated 5-row candidate pages under a frame small enough to force trimming, asserts each sent count, continuation presence, and exact concatenated row identity/order.
Re B2: DISPUTE — Both required safeguards already exist in the actual diff. `SessionRuntimeConfig::validate` rejects `session_list_snapshot_ttl_secs == 0`, with a regression assertion in `session_active_tool_limit_parses_and_must_be_positive`. `listing_cursor_is_connection_bound_and_expires` presents a valid cursor after TTL expiry and asserts `InvalidCursor`. ADR-010 explicitly documents that invalid, expired, or replayed cursors all fail as `invalid_cursor`; conflating these causes is the settled protocol contract so clients uniformly restart pagination.
Re B3: DISPUTE — This reopens a scope decision resolved by the task's prior consensus: the interim flat scan is explicitly accepted only when isolated in `spawn_blocking`, and task 1336 owns the shared multiprocess SQLite catalog/cache. Work is bounded operationally: remote admission caps concurrent connections, each connection's serve loop processes one list request at a time, snapshot entry capacity bounds retained memory, and subsequent pages do not rescan. Adding a second cache/index now would duplicate 1336 and introduce invalidation semantics immediately before replacement.
Re B4: DISPUTE — Remote visibility of minimal durable session IDs is intentional and pre-existing, not a new leak: before this change the same ListSessions operation exposed all durable IDs to any paired Observe client. It is required so a paired controller can discover and attach a persisted daemon-owned session. The settled task-1337 design explicitly says RemotePaired retains minimal `session_id/active/attached_clients` rows while forbidding workspace/model/preview/count/status. I strengthened the cursor test so a cursor minted in a LocalOwner snapshot is explicitly presented with RemotePaired trust on another connection snapshot and rejected as InvalidCursor.

### Revised Code / Diff
```rust
#[test]
fn frame_trimmed_pages_cover_snapshot_rows_once_in_order() {
    let rows = (0_u64..12).map(|index| SessionListEntry {
        session_id: format!("session-{index:04}"),
        active: true,
        attached_clients: 0,
        model: Some("model-with-enough-length-to-force-trimming".to_string()),
        updated_at_unix_ms: Some(100 - index),
        preview: Some("bounded preview content".to_string()),
        message_count: Some(2),
        turn_status: Some(TurnStatus::Idle),
    }).collect::<Vec<_>>();
    let expected = rows.iter().map(|row| row.session_id.clone()).collect::<Vec<_>>();
    let mut delivered = Vec::new();
    let mut offset = 0;
    while offset < rows.len() {
        let end = offset.saturating_add(5).min(rows.len());
        let remaining = rows.len() - offset;
        let (message, sent) = fit_session_list_to_frame(
            format!("page-{offset}"), None, rows[offset..end].to_vec(),
            remaining, format!("v1_cursor_{offset}"), 420,
        ).unwrap();
        let ServerMessage::SessionList { sessions, next_cursor, .. } = message else { panic!() };
        assert!(sent > 0);
        assert_eq!(sent, sessions.len());
        delivered.extend(sessions.into_iter().map(|row| row.session_id));
        offset += sent;
        assert_eq!(next_cursor.is_some(), offset < rows.len());
    }
    assert_eq!(delivered, expected);
}
```

```diff
- ConnectionTrust::LocalOwner,
+ ConnectionTrust::RemotePaired,
  &mut other_connection,
```

Focused pagination invariant test passes. Existing evidence referenced in disputes remains unchanged: zero-TTL config validation, expired-cursor test, spawn_blocking scan, bounded remote admission, and explicit remote-minimal privacy tests.

### New Points
R1. The prior task-1337 consensus is load-bearing scope: task 1336 owns catalog/cache mechanics, while 1337 may retain an interim spawn_blocking flat scan. Requiring a daemon-level cache here would duplicate the next task and create temporary invalidation behavior.
R2. Remote minimal durable-ID discovery is an intentional compatibility behavior needed for remote reattachment; privacy expansion is prevented by redacting every newly-added rich field and the workspace envelope.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S6. The new frame_trimmed_pages_cover_snapshot_rows_once_in_order test exercises fit_session_list_to_frame directly with a hand-rolled offset walk. Consider an additional integration-level variant that drives list_sessions_for_connection end-to-end (cursor round-trips through ConnectionListingSnapshot) with a tiny max_frame_bytes, so the offset/expected_cursor bookkeeping in the connection loop — not just the pure fitting function — is covered by the once-in-order invariant.
S7. When landing task 1336, revisit B3's residual observation: per-record std::fs::canonicalize during the durable scan should be eliminated by the SQLite catalog storing pre-canonicalized workspace identity, so filtering becomes an indexed lookup rather than filesystem syscalls.

### Responses to Prior Points
Re R1: Accepted. Task-ownership boundaries settled in prior consensus are load-bearing; the interim spawn_blocking scan with bounded remote admission, per-connection serialized list handling, and snapshot capacity limits keeps the interim cost bounded, and adding a daemon-level cache now would duplicate task 1336 and introduce throwaway invalidation semantics. I withdraw the demand for a cache/single-flight in this task.
Re R2: Accepted. Given that minimal durable-ID visibility to RemotePaired predates this diff and is required for remote reattachment discovery, this change does not widen the privacy surface — every newly added rich field and the workspace envelope are redacted for RemotePaired. The strengthened cross-trust cursor rejection test closes the remaining verification gap I asked for.
B1: resolved
B2: resolved
B3: resolved
B4: resolved

**Verdict:** APPROVE
