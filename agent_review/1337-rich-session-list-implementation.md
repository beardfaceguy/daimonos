# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1337-rich-session-list-implementation-20260827" -->

<!-- event id="request" artifact path="1337-rich-session-list-implementation/artifacts/round-1-review-request.diff" sha256="1597fbd3ce612890851cf10973072530e757912b297c1cee5e31200fa0dbb320" -->
## Review Request — Round 1
**Task:** 1337 — Rich daemon session-list metadata through protocol and client
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Implement the settled task-1337 design as an interim pre-catalog listing layer: explicit ConnectionTrust independent of capabilities; rich LocalOwner rows and minimal RemotePaired rows; canonical hashed workspace identity in the response envelope; UTF-8-safe bounded previews; provider-history message counts; optional live turn status; active last-activity timestamps; async spawn_blocking durable scans with legacy cwd canonicalization; newest-first deterministic ordering; one bounded, expiring, connection-local trust-neutral listing snapshot addressed by rotating versioned random cursors; typed invalid_cursor errors; frame fitting that preserves opaque cursors; typed rows through SessionClient, SessionController, and TuiSession; additive Android wire mirror.

### Relevant Code / Diff
Actual load-bearing implementation excerpts; the full uncommitted diff is present in the workspace paths named below.

```rust
// src/session_protocol.rs
pub enum ServerMessage {
    // ...
    SessionList {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<SessionWorkspace>,
        sessions: Vec<SessionListEntry>,
        next_cursor: Option<String>,
    },
}

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

pub struct ProtocolLimits {
    // ...
    pub max_identifier_bytes: usize,
    pub max_cursor_bytes: usize,
}
// ListSessions cursor validation uses max_cursor_bytes, not identifier bytes.
```

```rust
// src/session_daemon.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTrust { LocalOwner, RemotePaired }

#[derive(Clone)]
pub struct CapabilityPolicy {
    allowed: HashSet<ClientCapability>,
    trust: ConnectionTrust,
}

struct SessionEntry {
    // existing core/client/snapshot state...
    last_activity_unix_ms: Arc<AtomicU64>,
}

#[derive(Clone)]
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
                session_id: summary.id, active: false, attached_clients: 0,
                model: Some(summary.model),
                updated_at_unix_ms: summary.updated_at.map(system_time_unix_ms),
                preview: summary.first_user_line,
                message_count: Some(summary.message_count), turn_status: None,
            });
        }
    }
    let active: Vec<(String, Arc<SessionEntry>)> = self.sessions.lock()...iter()
        .map(|(id, entry)| (id.clone(), Arc::clone(entry))).collect();
    for (session_id, entry) in active {
        let durable = sessions.get(&session_id);
        let (message_count, preview) = match entry.core.session.try_lock() {
            Ok(session) => (session.history().len(), first_user_preview(session.history(), self.session_list_preview_bytes)),
            Err(_) => (durable.and_then(|r| r.message_count).unwrap_or_default(), durable.and_then(|r| r.preview.clone())),
        };
        sessions.insert(session_id.clone(), SessionListEntry {
            session_id, active: true,
            attached_clients: entry.clients.lock()...len(),
            model: Some(entry.core.current_model()),
            updated_at_unix_ms: Some(entry.last_activity_unix_ms.load(Ordering::Acquire)),
            preview, message_count: Some(message_count),
            turn_status: Some(entry.snapshot.lock()...snapshot.turn_status),
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
    &self, request_id: String, cursor: Option<&str>, trust: ConnectionTrust,
    snapshot: &mut Option<ConnectionListingSnapshot>, max_frame_bytes: usize,
) -> Result<ServerMessage, SessionListPageError> {
    if cursor.is_none() {
        *snapshot = Some(ConnectionListingSnapshot {
            rows: self.listing_rows().await?, offset: 0, expected_cursor: None,
            expires_at: Instant::now() + self.session_list_snapshot_ttl,
        });
    }
    let listing = snapshot.as_mut().ok_or(InvalidCursor)?;
    if Instant::now() >= listing.expires_at
        || cursor.is_some_and(|c| listing.expected_cursor.as_deref() != Some(c)) {
        *snapshot = None; return Err(InvalidCursor);
    }
    let end = (listing.offset + self.session_list_page_size).min(listing.rows.len());
    let rows = render_listing_rows(&listing.rows[listing.offset..end], trust);
    let remaining = listing.rows.len() - listing.offset;
    let next = format!("v1_{}", Uuid::new_v4());
    let workspace = (trust == LocalOwner).then(|| self.workspace_identity.clone()).flatten();
    let (message, sent) = fit_session_list_to_frame(
        request_id, workspace, rows, remaining, next, max_frame_bytes,
    ).map_err(TooLarge)?;
    listing.offset += sent;
    listing.expected_cursor = match &message { SessionList { next_cursor, .. } => next_cursor.clone(), _ => unreachable!() };
    if listing.expected_cursor.is_none() { *snapshot = None; }
    Ok(message)
}

fn render_listing_rows(rows: &[SessionListEntry], trust: ConnectionTrust) -> Vec<SessionListEntry> {
    rows.iter().cloned().map(|mut row| {
        if trust == RemotePaired {
            row.model = None; row.updated_at_unix_ms = None; row.preview = None;
            row.message_count = None; row.turn_status = None;
        }
        row
    }).collect()
}

fn fit_session_list_to_frame(
    request_id: String, workspace: Option<SessionWorkspace>,
    mut sessions: Vec<SessionListEntry>, remaining: usize,
    cursor: String, max_frame_bytes: usize,
) -> Result<(ServerMessage, usize), String> {
    loop {
        let next_cursor = (sessions.len() < remaining).then(|| cursor.clone());
        let message = ServerMessage::SessionList { request_id: request_id.clone(), workspace: workspace.clone(), sessions: sessions.clone(), next_cursor };
        if serde_json::to_vec(&message)?.len() <= max_frame_bytes { return Ok((message, sessions.len())); }
        sessions.pop();
        if sessions.is_empty() { return Err(...); }
    }
}
```

```rust
// serve_client_with_policy keeps one listing_snapshot local to the physical
// connection. CapabilityPolicy::local_trusted has LocalOwner; the remote
// serve_client_with_capabilities constructor always sets RemotePaired.
let mut listing_snapshot = None;
// ListSessions validates cursor with max_cursor_bytes, then:
match self.list_sessions_for_connection(
    request_id.clone(), cursor.as_deref(), capability_policy.trust,
    &mut listing_snapshot, limits.max_frame_bytes,
).await {
    Ok(message) => transport.send(&message).await?,
    Err(InvalidCursor) => send_error("invalid_cursor"),
    Err(CapacityExceeded) => send_error("session_list_capacity"),
    Err(TooLarge(message)) => send_error("session_list_too_large"),
}
```

```rust
// src/session_factory.rs
pub struct AgentSessionFactory {
    workspace: PathBuf, // canonicalized at construction
    workspace_identity: SessionWorkspace, // ws_<sha256 canonical path>, bounded basename label
    // ...
}

async fn persisted_session_summaries(&self, max_preview_bytes: usize) -> Vec<SessionSummary> {
    let store = self.store.clone();
    let workspace = self.workspace.clone();
    tokio::task::spawn_blocking(move || {
        store.list_with_preview_limit(max_preview_bytes).into_iter().filter(|summary| {
            summary.cwd.as_deref().and_then(|cwd| canonicalize(cwd).ok())
                .is_some_and(|cwd| cwd == workspace)
        }).collect()
    }).await.unwrap_or_default()
}
```

```rust
// src/session_store.rs
pub(crate) fn first_user_preview(messages: &[Message], max_bytes: usize) -> Option<String> {
    // selects first user text line and passes it to normalize_preview
}
fn normalize_preview(line: &str, max_bytes: usize) -> Option<String> {
    let mut preview = String::new();
    for word in line.split_whitespace() {
        // append one normalized separator and only a floor_char_boundary slice
        // while preview.len() remains <= max_bytes
    }
    (!preview.is_empty()).then_some(preview)
}
```

```rust
// src/session_client.rs
pub enum SessionClientOutcome {
    SessionList { request_id: String, workspace: Option<SessionWorkspace>, sessions: Vec<SessionListEntry>, next_cursor: Option<String> },
    ServerError { request_id: Option<String>, code: String, message: String },
    // ...
}
// ServerMessage::Error during an attached session is a nonterminal typed
// outcome rather than a transport failure; attach-handshake Error stays fatal.
pub async fn list_sessions(&mut self, cursor: Option<String>) -> Result<String, SessionClientError> {
    self.require(ClientCapability::Observe)?;
    let request_id = self.next_request_id("list");
    self.transport.send(ClientMessage::ListSessions { request_id: request_id.clone(), cursor }).await?;
    Ok(request_id)
}
```

```rust
// src/session_controller.rs and src/tui/session.rs
SessionControllerCommand::ListSessions { cursor } forwards to SessionClient and
blocks atomic session switching while in flight. TuiSession records the local
CommandAccepted request id, clears the bounded pending-operation maps on either
SessionList or correlated ServerError, and exposes take_session_list() returning
(workspace, rows, next_cursor).
```

```kotlin
// android/.../WireModels.kt
ServerMessage.SessionList adds nullable workspace. SessionListEntry adds nullable
model, updatedAtUnixMs, preview, messageCount, and turnStatus. All are optional,
so minimal remote and older daemon rows still decode.
```

Tests in the actual diff cover: dedicated cursor limit; additive Rust wire roundtrip; UTF-8 preview normalization/cap; legacy cwd alias canonicalization; async filtered summaries; active provider-history metadata/activity; local-vs-remote trust both directly and through serve policy; bounded snapshot pagination; connection-bound, replayed, expired, oversized and unknown cursors; frame trimming preserving opaque cursor; capacity failure instead of silent omission; nonfatal invalid_cursor connection; typed rows through SessionClient and TuiSession; Kotlin rich/minimal decoding.

Verification: cargo fmt clean; cargo clippy --all-targets -- -D warnings clean; cargo test 1724 passed, 10 ignored; pytest 296 passed. Android ./gradlew test could not start because no java executable/JAVA_HOME exists on this machine.

### Known Concerns
1. Interim SessionStore listing still parses every persisted file per new pagination snapshot; it runs in spawn_blocking, while task 1336 owns the bounded SQLite catalog replacement.
2. When an active AgentSession is locked by a turn, rich listing uses the last durable provider-history count/preview (or zero/None for a never-persisted first turn) rather than blocking for the full LLM turn.
3. The connection snapshot hard-fails with session_list_capacity rather than silently hiding rows when the configured row cap is exceeded.
4. Changing attached-session ServerMessage::Error from a fatal SessionClientError into a nonterminal ServerError outcome affects all daemon commands, not only listing; attach-handshake errors remain fatal.
5. Android Kotlin tests were added but could not run locally due missing Java.

### Specific Questions for Reviewer
1. Does the per-connection snapshot and trust filtering fully enforce the settled B7 privacy boundary?
2. Are ordering, cursor rotation, frame trimming, replay/expiry rejection, and capacity behavior free of omission/duplication bugs within one snapshot?
3. Is the active-session try_lock fallback semantically honest enough for provider-history message_count, or should the field be absent while the core is busy?
4. Is making attached-session ServerMessage::Error a nonterminal typed outcome correct for reconnect behavior and pending-operation cleanup?
5. Are any protocol compatibility or Android mirror changes missing?

<!-- event id="review_response_1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. Busy active sessions publish stale metadata as authoritative. prompt_with_active_turn holds the session mutex across the provider turn (src/session_core.rs:1162-1235). During that potentially minutes-long interval, listing_rows falls back to durable metadata and emits Some(0)/an old count plus an old or absent preview (src/session_daemon.rs listing_rows). A new session’s first active prompt therefore reports zero messages and no preview despite already emitting UserMessage. Return None for unavailable message_count and derive preview from the live snapshot, or maintain equivalent accurate live metadata. Add a locked-session regression test; the current test only exercises an unlocked core.

### Non-Blocking Suggestions
S1. Execute the added Android fixture test once Java is available. The Kotlin mirror appears schema-compatible, but ProtocolFixtureTest.kt remains unverified on this machine.

**Verdict:** REVISE

<!-- event id="rebuttal_1" artifact path="1337-rich-session-list-implementation/artifacts/round-1-rebuttal.diff" sha256="dbe08424ba9687f78ce24b2ec3d80c5d1196b7e8f989671b92289224c900b5be" -->
## Rebuttal — Round 1
**Position:** AGREE

### Responses to Blocking Concerns
Re B1: ACCEPT — The fallback mislabeled stale durable/zero data as current provider-history metadata. Active listing now takes one canonical snapshot lock first. If AgentSession::try_lock succeeds, message_count and preview come from exact provider history. If the turn holds the lock, message_count is None (unknown) and preview is derived from the canonical live UserMessage snapshot with the same whitespace normalization and UTF-8 byte bound. A regression test holds the AgentSession lock, emits a live user event, and asserts None count plus the live bounded preview.

### Revised Code / Diff
```diff
--- a/src/session_daemon.rs
+++ b/src/session_daemon.rs
@@
-            let durable = sessions.get(&session_id);
+            let (turn_status, snapshot_preview) = {
+                let snapshot = entry.snapshot.lock().unwrap_or_else(|p| p.into_inner());
+                let preview = snapshot.snapshot.transcript.iter()
+                    .find(|entry| entry.role == TranscriptRole::User)
+                    .and_then(|entry| entry.text.lines().next())
+                    .and_then(|line| session_store::normalize_preview(
+                        line, self.session_list_preview_bytes,
+                    ));
+                (snapshot.snapshot.turn_status, preview)
+            };
             let (message_count, preview) = match entry.core.session.try_lock() {
                 Ok(session) => (
-                    session.history().len(),
+                    Some(session.history().len()),
                     session_store::first_user_preview(
                         session.history(), self.session_list_preview_bytes,
                     ),
                 ),
-                Err(_) => (
-                    durable.and_then(|row| row.message_count).unwrap_or_default(),
-                    durable.and_then(|row| row.preview.clone()),
-                ),
+                Err(_) => (None, snapshot_preview),
             };
-            let turn_status = entry.snapshot.lock()...snapshot.turn_status;
@@
-                    message_count: Some(message_count),
+                    message_count,
                     turn_status: Some(turn_status),
```

```diff
--- a/src/session_store.rs
+++ b/src/session_store.rs
@@
-fn normalize_preview(line: &str, max_bytes: usize) -> Option<String> {
+pub(crate) fn normalize_preview(line: &str, max_bytes: usize) -> Option<String> {
```

```rust
#[tokio::test]
async fn busy_active_listing_marks_count_unknown_and_uses_snapshot_preview() {
    let daemon = SessionDaemon::new(1, 1, 8, 32);
    let core = test_core();
    daemon.create_session("session-a".to_string(), Arc::clone(&core)).unwrap();
    core.events.emit(SessionEvent::UserMessage {
        text: "  live   prompt\nignored".to_string(),
        request_id: Some("prompt-1".to_string()),
    }).unwrap();
    // Wait for canonical snapshot projection, then hold AgentSession lock.
    let _session_guard = core.session.lock().await;
    let rows = daemon.listing_rows().await.unwrap();
    assert_eq!(rows[0].message_count, None);
    assert_eq!(rows[0].preview.as_deref(), Some("live prompt"));
}
```

Focused regression and Clippy pass.

### New Points
None.

**Requesting:** RE-REVIEW

<!-- event id="review_response_2" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
B1: resolved

**Verdict:** APPROVE
