# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="1335-attach-denied-code-20260824" -->

<!-- event id="request" artifact path="1335-attach-denied-code/artifacts/round-1-review-request.diff" sha256="7b51d744f94d6511127db7133f52eb233d14bde4f5bd389f1bb9df4f5812ed72" -->
## Review Request — Round 1
**Task:** 1335 — Additive typed AttachDenied codes prerequisite
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Land only the previously reviewed prerequisite slice of task 1335 before session switching: add optional AttachDenied.code with a stable enum while preserving legacy reason-only decode and protocol v2. Map every daemon handshake/open/attach/snapshot denial to a privacy-safe code. SessionClient retains code in its typed error. Android mirrors nullable typed codes. This enables tasks 1333/1339 to branch on ClientLimitReached without parsing free text; the remainder of task 1335 durability/store errors stays open.

### Relevant Code / Diff
```rust
// session_protocol.rs
pub enum ServerMessage {
    AttachDenied {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<AttachDeniedCode>,
        reason: String,
    },
}
#[serde(rename_all = "snake_case")]
pub enum AttachDeniedCode {
    InvalidMessage, ProtocolVersion, SessionNotFound, SessionStopped,
    SessionLimitReached, ClientLimitReached, DuplicateClient,
    SessionAlreadyActive, SessionOpenFailed, FactoryUnavailable,
    EventSubscriptionFailed, DaemonShuttingDown, SnapshotTooLarge,
}
```

```rust
// session_daemon.rs
fn session_daemon_error_code(error: &SessionDaemonError) -> AttachDeniedCode {
    match error {
        DuplicateSession(_) => SessionAlreadyActive,
        SessionLimitReached {..} => SessionLimitReached,
        SessionNotFound(_) => SessionNotFound,
        SessionStopped(_) => SessionStopped,
        DuplicateClient(_) => DuplicateClient,
        ClientLimitReached {..} => ClientLimitReached,
        EventSubscription(_) => EventSubscriptionFailed,
        FactoryUnavailable => FactoryUnavailable,
        OpenFailed(_) => SessionOpenFailed,
        ShuttingDown => DaemonShuttingDown,
    }
}

// All AttachDenied send sites add exact code:
// malformed/first-message -> InvalidMessage
// version mismatch -> ProtocolVersion
// open/attach errors -> mapping above
// stopped during attach -> SessionStopped
// fitted snapshot failure -> SnapshotTooLarge
```

```rust
// session_client.rs
pub enum SessionClientError {
    AttachDenied { code: Option<AttachDeniedCode>, reason: String },
}
ServerMessage::AttachDenied { code, reason } =>
    Err(SessionClientError::AttachDenied { code, reason })
```

Android WireModels adds nullable AttachDeniedCode with matching serial names. Tests pin exact ClientLimitReached wire shape, legacy reason-only decode in Rust and Android, InvalidMessage mapping, and ClientLimitReached daemon mapping. Focused tests and cargo check pass.

### Known Concerns
1. SessionOpenFailed remains intentionally coarse until task 1335 implements typed store/version/corruption/workspace failures.
2. Code is an additive field; reason remains for human diagnostics and legacy clients.
3. This section does not yet change TUI behavior; it provides the typed prerequisite consumed by task 1333 switching.

### Specific Questions for Reviewer
1. Are the code categories stable/privacy-safe and exhaustive for current daemon errors?
2. Is retaining protocol v2 valid for this optional field across Rust and Android clients?
3. Should any current error map differently before downstream switching consumes ClientLimitReached?

<!-- event id="review_response" reviewer.model="claude-fable-5" reviewer.provider="anthropic" -->
## Review Response — Round 1
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
S1. Answering Q1: the enum looks exhaustive against the current SessionDaemonError variants plus the handshake/snapshot paths, and none of the codes leak session names, paths, or client identifiers, so the set reads as privacy-safe. Consider adding a #[non_exhaustive] attribute (or documenting a forward-compat 'unknown code' decode policy on clients) so future codes added under the remainder of task 1335 do not break older Rust/Android decoders that match exhaustively on AttachDeniedCode.
S2. Answering Q2: keeping protocol v2 is valid because the field is Option + skip_serializing_if on the wire and nullable on Android, and legacy reason-only decode is pinned by tests on both platforms. Suggest also adding one round-trip test where a v2 peer sends an unrecognized future code string, to lock in the lenient-decode behavior rather than assuming serde's default failure mode.
S3. Answering Q3: no remapping needed before task 1333/1339 consume ClientLimitReached; the DuplicateClient vs ClientLimitReached distinction is the one downstream switching cares about and it is mapped directly. SessionOpenFailed staying coarse is acceptable per the stated known concern; recommend leaving a code comment or task link at the mapping site noting it will be subdivided by the remaining 1335 work so the coarse variant is not accidentally treated as stable semantics.

**Verdict:** APPROVE
