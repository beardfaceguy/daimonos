# ADR-010: Daemon-owned agent sessions, local TUI, and remote control

**Date:** 2026-07-26  
**Status:** Accepted  
**Tracks:** Vikunja project 183, epic #1092; research #1090; TUI #1091  
**Builds on:** ADR-002 (compaction), ADR-003 (ACP-MCP bridge), ADR-006
(observability), ADR-009 (agent coordination)

## Context

Daimonos currently has three agent frontends with overlapping but different
lifetimes:

- `daimonos agent` owns a one-shot agent loop and exits after one task;
- `daimonos chat` owns an `AgentSession` inside a Reedline REPL;
- `daimonos acp` owns a multi-session `AcpState`, but the whole engine is tied
  to one stdio ACP client connection and tears down when stdin closes.

The requested product has two clients for one live session:

1. a persistent full-screen local terminal UI built with Ratatui + Crossterm;
2. a native Android controller connected through public reverse-proxied WSS.

Closing the local terminal must detach the UI without ending the session. A
remote client can send prompts, interrupt, and answer tool approvals, including
`AllowAlways` only when the host configuration opts in. Both clients must see
the same transcript, tool lifecycle, runtime options, and context usage.

The implementation must preserve existing ACP behavior, reuse existing provider
and tool abstractions, proceed in small compiling slices, and use tests-first
red/green development.

## Existing seams we keep

### `LlmProvider`, not a new `LlmBackend`

`src/providers/mod.rs::LlmProvider` already has multiple real implementations
(Anthropic, OpenAI, OpenRouter), a streaming-first `stream` method, normalized
`ThinkingLevel`, image capability, and context-window resolution. Adding a
second backend trait would create adapters around adapters without a new
variation point. Model discovery and runtime-option metadata extend
`LlmProvider` when implemented.

### Tool facade and plugins, not a universal new `Tool` trait

Native tools already flow through `tool_facade` into compact opcodes; dynamic
plugins implement `ToolPlugin`; remote MCP tools flow through `McpBridge`.
Execution is already separated from approval by the async before-tool hook.
The daemon will replace the frontend-specific approval callback with an
approval broker, but it will not force every opcode into a new trait until a
second execution implementation requires one.

### ACP remains a supported adapter

ACP already defines the correct interaction semantics: session creation/load,
prompt, cancel, streamed updates, tool lifecycle, plans, model configuration,
and permission requests. Existing Zed clients continue to speak ACP.

The daemon core will use a transport-neutral canonical event/state model. The
ACP adapter maps that model to/from ACP v1. Local TUI and WSS clients use the
versioned plain-serde protocol in `session_protocol.rs`. This avoids exposing
SDK connection objects or ACP crate version details to Android while preserving
one set of session semantics.

### Agent mail is not a stream transport

ADR-009's SQLite mailbox is a durable, bounded coordination control plane. It
is not used for token deltas, tool output, approvals, or reconnect replay. A
future bus transport is not introduced until a real low-latency bus
implementation is scheduled.

## Decision

### D1 — A long-lived daemon owns every interactive `AgentSession`

Interactive sessions are created inside one daemon process. The daemon owns:

- provider and `AgentSession`;
- canonical history and usage;
- tool execution and MCP bridges;
- current model, mode, effort, context/compaction policy;
- active turn cancellation;
- tool-call state;
- pending approvals;
- monotonic event sequence and bounded replay ring;
- attached-client registry and capabilities;
- session persistence and lifecycle.

The local terminal and Android application are clients. No client owns the
provider or tools.

`daimonos agent` on a TTY starts or finds the daemon, creates/loads a session,
and attaches the TUI. Explicit print/non-interactive mode retains one-shot
behavior for scripts and CI. `daimonos chat` becomes a compatibility alias or
legacy fallback; it does not remain a separate interactive agent architecture.

### D2 — Detach and stop are different operations

Disconnect, terminal closure, `/detach`, network loss, or Android process death
remove only that client. They do not stop the session.

A session ends only on:

- explicit `stop_session` from an authorized client;
- configured retention/idle policy;
- daemon shutdown;
- unrecoverable daemon process failure.

The default interactive policy is no idle timeout (`0` means persist until
explicitly stopped). Only completed turns are durable across daemon restart;
an in-flight generation/tool is cancelled and represented honestly on restart.

### D3 — One canonical protocol model, trait-free wire data

`ClientMessage`, `ServerMessage`, `SessionEvent`, `SessionSnapshot`, runtime
options, context usage, and capability enums are plain serde data. They contain
no transport or frontend traits.

Protocol version 1 uses:

- fine-grained capabilities: `Observe`, `Prompt`, `Interrupt`, `ApproveOnce`,
  `ApproveAlways`;
- monotonic per-session event sequence numbers;
- canonical snapshots for attach/reconnect;
- bounded validation for every untrusted string/list before dispatch;
- summarized tool input by default and explicit tool output as an acknowledged
  sensitive surface.

Unknown object fields are tolerated for additive compatibility. Unknown message
variants are rejected until a version/capability negotiation allows them.

### D4 — `SessionCore` is extracted from ACP, not rewritten

The first core refactor extracts `AcpState`/`SessionHandle` behavior into a
transport-independent `SessionCore` and `SessionHandle` module. Existing ACP
handlers become an adapter over that core.

Extraction order is mandatory:

1. characterize current ACP behavior with tests;
2. move the existing behavior without semantic changes;
3. route stdio ACP through the extracted core;
4. only then add additional clients/transports.

Provider construction, MCP bridge creation, persistence, cancellation, model
preparation, usage updates, and tool-call liveness are reused rather than
reimplemented.

### D5 — Events and approvals use separate reliability paths

Session events are fan-out data. A bounded broadcast/replay mechanism is
appropriate, but `tokio::broadcast` alone is not recovery: lagged clients request
missing retained deltas or receive a canonical snapshot.

Approvals are reliable requests with exactly one terminal resolution. An
`ApprovalBroker` owns pending requests and enforces:

- first valid response wins atomically;
- duplicate/late responses return `approval_already_resolved`;
- only clients with the required capability may answer;
- remote `AllowAlways` is accepted only when host configuration enables it;
- local policy blocks cannot be overridden by any client;
- if every eligible client disappears, a bounded timeout denies safely;
- resolution is emitted to all observers regardless of which client answered.

The local TUI retains unconditional revoke and stop authority. One remote
controller is allowed in v1; additional remote clients are deferred.

### D6 — Client concurrency is explicit

Only one turn may run per session. A prompt received while the session is busy
is rejected with `session_busy`; prompts are not silently queued. This avoids
ambiguous ordering between terminal and Android.

Interrupt is idempotent. Local and remote clients may both request it if granted.
Configuration changes apply only at a safe boundary before the next generation.
They never mutate an in-flight provider request.

### D7 — `ClientTransport` is introduced with two implementations

The daemon sees only `ClientMessage`/`ServerMessage` through:

```rust
#[async_trait]
pub trait ClientTransport: Send {
    async fn send(&mut self, message: &ServerMessage) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError>;
    fn peer_label(&self) -> &str;
}
```

The first implementations are:

- in-memory transport for deterministic integration tests;
- Unix-domain socket transport for the local TUI.

WSS is added after the daemon/TUI path is stable. Per-client tasks may be generic
on a transport; heterogeneous registries use `Box<dyn ClientTransport>`.
The daemon never matches on transport type.

The UDS is mode `0600`, rejects unsafe pre-existing paths, and verifies peer
credentials/ownership where supported. Local filesystem access is not the only
check when peer credentials are available.

### D8 — `Frontend` shares state reduction, not rendering

A common frontend module owns a pure state reducer for:

- snapshots and sequence tracking;
- transcript and partial assistant/thought chunks;
- tool-call state;
- pending/resolved approvals;
- runtime options and selected values;
- context usage;
- connection/reconnect status.

`HeadlessFrontend` and `TuiFrontend` are the first two implementations of the
frontend variation point. Ratatui rendering and key handling remain in
`TuiFrontend`; protocol consumption and recovery are not duplicated.

Android is not a Rust `Frontend` implementation, but consumes the same versioned
wire schema and contract fixtures.

### D9 — Ratatui + Crossterm is the local interactive stack

The TUI uses Ratatui for layout/rendering and Crossterm for raw mode, alternate
screen, input, paste, resize, cursor, and terminal restoration.

Required panels/behavior:

- scrollable transcript;
- streamed assistant/thought output;
- structured expandable tool/diff/terminal cards;
- approval modal;
- multiline composer and interrupt;
- model/mode/effort/context settings;
- context percentage and usage/cost;
- coordination-mail and remote-control status;
- detach, stop, pair, and revoke commands.

An RAII terminal guard restores canonical mode, cursor, mouse/paste modes, and
alternate screen on success, error, Ctrl-C, and panic. Model/tool text is
escaped/sanitized so ANSI/OSC sequences cannot control the host terminal.
Non-TTY invocation never emits control sequences and selects explicit
print/line behavior.

### D10 — Runtime options are daemon-owned typed data

The daemon publishes a typed option schema rather than clients hard-coding
provider branches. Required controls include:

- provider-discovered model;
- hard-enforced `Agent`, `Plan`, and `Ask` modes;
- supported reasoning/effort levels;
- adjustable provider window when real, or truthfully labeled internal
  `Context budget` when it is only a lower compaction budget;
- supported output limit/temperature/fast/reasoning-summary options.

Clients send only option id/value pairs. The daemon validates, persists, applies
at the next safe boundary, refreshes options after a model/provider change, and
broadcasts the canonical result.

Plan/Ask restrictions are tool-policy gates, not prompt-only suggestions.
Runtime options cannot widen the host safety policy.

### D11 — Context utilization is canonical daemon state

The displayed numerator is current prompt occupancy for the next generation,
not cumulative billed tokens. Primary percentage uses effective input budget:

`model context window - output reservation`.

The detail view exposes full provider window, reservation, compaction
thresholds, and last compaction. Unknown windows display unknown rather than a
fabricated percentage. Invalid reservation/window configuration is explicit.
The daemon computes integer basis points and all clients render the same value.

### D12 — Public WSS is loopback-bound behind the reverse proxy

The daemon binds the remote HTTP/WSS service to loopback by default. The
existing reverse proxy is the public TLS terminator and only public ingress.
Daimonos trusts forwarded headers only from configured proxy peers.

Public unauthenticated endpoints disclose no session id, workspace path, model,
agent identity, or pairing state. Both proxy and daemon enforce connection,
pairing-attempt, rate, frame-size, queue, and idle limits.

Native Android clients are not authenticated by browser `Origin`. A future web
client must use a strict Origin allowlist.

### D13 — Pairing uses local consent and device proof

`Authenticator` is added when both `LocalTrust` and `PairingAuth` exist.

Remote pairing uses:

1. high-entropy single-use claim, five-minute default TTL, bounded attempts;
2. claim submitted in the first authenticated exchange, not a long-lived URL
   query token;
3. remote device public key and locally displayed fingerprint;
4. explicit local approve/deny and capability selection;
5. random in-memory session ticket bound to proof of the device key;
6. immediate revoke/kick and connection close;
7. daemon death revokes all tickets.

PASETO/JWT is not used in v1 because the daemon already keeps revocation and
client state. Opaque tickets reduce parser/algorithm surface. A stateless token
can be reconsidered only if a distributed verifier becomes real.

### D14 — Remote approvals never widen host policy

Host configuration defaults:

```toml
[remote_control.approvals]
remote_approve_once = true
remote_allow_always = false
```

The server omits `ApproveAlways` from granted capabilities and available
approval choices when disabled, and rejects a forged response. The Android app
cannot change the host setting.

### D15 — Security projection is explicit, not absolute

A remote transcript can contain source text, paths, commands, diffs, terminal
output, raw tool output, or secrets. We do not claim that files never cross.
Instead:

- tool start events contain a bounded summary unless policy explicitly exposes
  raw input;
- tool output is an explicit sensitive event surface and is bounded;
- no ambient filesystem/env enumeration exists in the protocol;
- logs and telemetry never record tickets, prompts, tool arguments/results, or
  approval detail;
- Android/TUI render all model/tool strings as untrusted text;
- attachment upload is a separate future design.

## Threat model

### Protected assets

- host command execution and filesystem access;
- provider/MCP/API credentials;
- source and transcript contents;
- approval authority and persistent `AllowAlways` policy;
- session availability and daemon memory/disk.

### Adversaries

- unauthenticated internet scanner through the public proxy;
- attacker with leaked pairing URL/claim;
- malicious or compromised paired Android client;
- malicious model/tool output attempting terminal/UI injection;
- slow or flooding client causing memory/CPU exhaustion;
- stale/replayed approval/config/prompt message;
- same-host unprivileged process attacking the UDS.

### Required controls

- TLS at proxy, loopback backend;
- single-use/expiring/rate-limited pairing;
- device proof, explicit local consent, per-message capability checks;
- bounded frames/fields/queues/replay/history projections;
- monotonic sequence and request ids; idempotent terminal operations;
- origin checks for future browser clients, but never as primary auth;
- output sanitization and safe rendering;
- metadata-only security logs;
- fail-closed privileged dispatch, fail-open optional coordination/telemetry;
- tests for every authorization and lifecycle transition.

## Rejected alternatives

### Put remote control on agent mail

Rejected: SQLite polling is unsuitable for live token streams and reliable
approval round trips. It also has no cross-machine replication today.

### Build a new `LlmBackend`

Rejected: duplicates `LlmProvider` and three adapters.

### Build a generic `Tool` hierarchy now

Rejected: variation already exists in facade/plugins/MCP; approval routing is the
missing seam.

### PASETO/JWT in v1

Rejected: no distributed verifier; opaque in-memory tickets are smaller and
naturally die with the daemon.

### Daemonize a live terminal-owned session only when remote mode starts

Rejected: safely transferring an in-flight provider request, tool process,
approval, history, and MCP children between process lifetimes is fragile. The
daemon owns the session from creation.

### Queue simultaneous prompts

Rejected for v1: hidden ordering between terminal and Android is surprising.
Return `session_busy` and let the client retry after the terminal event.

## Implementation sequence

1. Plain wire data and validation (#1102).
2. Extract `SessionCore` from ACP and preserve stdio semantics (#1097).
3. In-memory + UDS `ClientTransport` (#1099).
4. Daemon lifecycle and local attach/detach (#1096).
5. Shared reducer + `HeadlessFrontend` (#1094).
6. Ratatui/Crossterm shell (#1101).
7. Tool/approval/runtime-option/context UX (#1093).
8. Replay ring/snapshot recovery (#1100).
9. `Authenticator`, public WSS, Android contract (#1095).
10. Native Android controller (#1103).

Every code slice starts with a failing test, lands as a compiling commit, runs
the full existing suite, receives code review, and is merged before the next
large dependency builds on it.

## Consequences

### Positive

- One authoritative agent loop serves Zed, terminal, tests, and Android.
- Terminal closure no longer kills interactive sessions.
- Existing ACP/provider/tool code is reused instead of forked.
- Security and capability decisions live server-side.
- Headless/in-memory paths make the daemon deterministic to test.

### Negative

- `daimonos chat` and `daimonos agent` CLI compatibility needs a migration.
- ACP's connection-specific permission routing requires careful extraction.
- Persistent daemons add process/socket/session cleanup responsibilities.
- Public remote control substantially increases security review and operational
  burden even with a reverse proxy.
- The plain external protocol must be versioned and maintained for Android.
