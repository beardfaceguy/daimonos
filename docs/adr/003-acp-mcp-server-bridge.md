# ADR-003: Consume Zed-provided MCP servers in ACP sessions

**Date:** 2026-07-18
**Status:** Accepted
**Tracks:** vikunja #990 (consume `mcp_servers` from Zed), #1008 (shared client pool), #982 (Zed
integration analysis, precursor)
**Relates to:** ADR-001 (provider boundary),
the tool facade (`src/tool_facade.rs`) and ACP frontend (`src/acp_cmd.rs`)

## Problem

Zed forwards every context server the user has configured to the ACP agent on `session/new`,
`session/load`, and `session/resume`, as `acp::McpServer` entries
(`~/zed/crates/agent_servers/src/acp.rs`, `mcp_servers_for_project`):

- `McpServer::Stdio { name, command, args, env }`
- `McpServer::Http { name, url, headers }`

Daimonos receives these on `NewSessionRequest.mcp_servers` / `LoadSessionRequest.mcp_servers`
(`Vec<McpServer>`) but **ignores the field entirely**. The consequence: a daimonos ACP session
cannot use the user's Zed MCP ecosystem (Linear, GitHub, filesystem servers, etc.). This ADR adds
a per-session **MCP bridge**: daimonos acts as an MCP *client* to each forwarded server, discovers
its tools, exposes them to the model with collision-safe names, dispatches tool calls, and records
usage in analytics — without disturbing native daimonos tools or the other frontends.

## Constraints and existing structure

- The agent loop (`src/agent.rs`) turns `StopReason::ToolUse` into calls dispatched through
  `tool_facade::invoke_with_progress(session, name, input, ...)`. That facade only knows
  opcode-backed native tools; when it returns `None` the loop currently emits
  `"tool '{name}' not available in agent mode"`. **That `None` branch is the remote-dispatch seam.**
- The tool list handed to the model is a fixed `Vec<ToolSchema>` built once in
  `acp_cmd::build_agent_config` from `tool_facade::active_schemas` (native tools, with OnDemand
  exclusion and per-tool context checks preserved).
- ACP already isolates sessions: each `SessionHandle` (`src/acp_cmd.rs`) owns its own session lock,
  cancel slot, connection cell, and model cell. A per-session bridge fits this model directly.
- `rust-mcp-sdk = "0.9"` is already a dependency (server+stdio). It also ships a full **client**
  (`client` feature + `stdio`/`streamable-http`/`sse` client transports) with `list_tools` /
  `call_tool`, `StdioTransport::create_with_server_launch(command, args, env)`, and HTTP transports
  that accept custom headers — an exact match for Zed's stdio/http shapes.

## Decisions

### D1 — Reuse the in-tree `rust-mcp-sdk` client (no new crate)

Enable additional **features** on the existing `rust-mcp-sdk`: `client` (empty feature, no extra
transitive deps), `streamable-http` (Zed's `Http` transport), and `sse` (legacy remote MCP). No new
top-level crate is added; `streamable-http`/`sse` pull a few transitive crates (`http`,
`http-body`, `http-body-util`). Approved by the maintainer 2026-07-18. Hand-rolling a JSON-RPC/SSE
client was rejected: it duplicates protocol code the SDK already provides and that the daimonos
*server* half already relies on.

### D2 — Shared client pool with per-session leases and routes

Each ACP session still builds its own bridge, tool names/routes, permission flow, and analytics
attribution from that session's forwarded `mcp_servers`. Identical transport configurations
(stdio command+args+sorted env, or HTTP URL+sorted headers) acquire leases on one process-wide
initialized client and cached `tools/list` result (#1008). Display names are excluded from the key,
so sessions may alias one server differently without spawning duplicate processes/connections.

Acquisition is synchronized per key: concurrent sessions deduplicate the same server while
different servers still connect concurrently. Failed connections are not cached. Releasing a
session decrements explicit lease counts; the last release shuts down the runtime and reaps stdio
children. `shared_pool_enabled=false` gives a bridge a private pool for servers that intentionally
require per-chat state isolation.

### D3 — Fail-open per server (failure isolation)

Building the bridge never fails `session/new`/`session/load`. Each server is initialized
independently with a bounded timeout; a server that fails to spawn, connect, initialize, or list
tools is logged and **skipped**. The session proceeds with the remaining servers plus all native
tools. One misconfigured server can never wedge a session. A remote `tools/call` that errors or
times out returns an error *tool result* to the model (so it can recover) — it does not abort the
turn.

Server handshakes run concurrently with bounded fan-out (`max_concurrent_connects`). Completed
connections are collected and then registered in their original forwarded-server order, so network
timing cannot change collision suffixes or route names (#1013).

A shared client's runtime failure can make calls fail in every leasing session, but each failure is
still returned as an error tool result and never aborts the ACP process. The pool does not
transparently reconnect or replay server state; a later acquisition retries only after the failed
entry has no remaining leases and is released.

### D4 — Collision-safe tool names: `mcp__{server}__{tool}`

Remote tools are exposed to the model as `mcp__{server}__{tool}` (the widely-used Zed/Claude
convention), sanitized to the provider tool-name constraint (`^[a-zA-Z0-9_-]{1,64}$`, non-conforming
chars → `_`) and truncated to 64 chars. Resolution order for collisions:

1. **Native tools always win.** A remote tool whose namespaced name collides with a native tool
   name is dropped (logged). Native names are short and unprefixed, so real collisions require a
   server literally named `mcp` with a matching tool — vanishingly rare, but handled.
2. **Remote-vs-remote** collisions (after sanitize/truncate) are de-duped deterministically: first
   registration wins; later duplicates get a numeric suffix (`…__2`) or are dropped if that also
   collides. Deterministic ordering = the order Zed sent the servers, then the server's tool order.

The mapping from namespaced name → (server, original tool name) is stored in the bridge so dispatch
can call the server with the *original* tool name.

### D5 — Dispatch seam: `AgentConfig.remote_tool_dispatch` hook

Add an optional async dispatch hook to `AgentConfig` (alongside the existing `before_tool_call`,
`after_tool_call`, `on_tool_progress` hooks). Signature (conceptual):

```
Fn(name: &str, input: &Value) -> Future<Output = Option<RemoteToolOutcome>>
```

The agent loop consults it **only** when `tool_facade::invoke` returns `None`, before the
"not available" fallback. `Some(outcome)` → the remote result (text content + is_error) is used as
the `ToolResult`; `None` → fall through to the existing "not available" message. Keeping this in
`AgentConfig` (not in `crate::session::Session`) means:

- Core `Session` stays MCP-free (opcode facade unchanged; MCP-server mode and CLI frontends
  unaffected).
- The bridge is wired **only** in the ACP frontend, which is the only place `mcp_servers` arrives.
- `before_tool_call` / `after_tool_call` still run for remote tools (permission + diff/terminate
  hooks), because they run in the loop *before* dispatch is chosen.

### D6 — Native tools first, remote appended; lazy filtering preserved

`build_agent_config` builds native `ToolSchema`s from `active_schemas` exactly as today (OnDemand
exclusion + context checks intact), then **appends** the bridge's remote schemas. Remote tools are
always exposed (the user explicitly configured them in Zed; there is no lazy/context notion for
them). Ordering: native first keeps native tools stable at the head of the list.

### D7 — Permissions and safety

Remote tool calls flow through the existing `before_tool_call` hook, so Zed's approval UI fires
just as it does for native destructive tools. Because `safety::is_destructive_tool` is
native-name-based and cannot reason about arbitrary remote tools, **all remote (`mcp__*`) tools are
treated as permission-required by default** — the safest default for third-party code. The existing
approval mode still governs (e.g. an "always allow" / non-interactive mode bypasses prompts as it
does for native tools; `remember_always` works per namespaced name). This is implemented by making
the safety gate treat the `mcp__` prefix as destructive-by-default.

### D8 — Lifecycle: init, shutdown, cancellation

- **Initialize** on `session/new` and on the `session/load` *rebuild* path (case 2 — process was
  restarted). Each client does the MCP `initialize` handshake + `tools/list`, bounded by
  `[acp.mcp] init_timeout_secs`.
- **Advertise capability** at ACP `initialize`: add
  `AgentCapabilities.mcp(McpCapabilities::new().stdio(..).http(..))` so Zed is spec-correct in
  forwarding `Http` servers (Zed gates `Http` on `session.mcp.http`). Gated by config so it can be
  disabled.
- **Shutdown** the bridge (all clients: `shut_down()`, and stdio child processes reaped) on
  `session/delete` and on process exit / `Drop`. This satisfies the resource-lifecycle rule (every
  spawned child + client has a teardown path).
- **Cancellation:** a `session/cancel` (or `session/delete` mid-turn) best-effort-cancels in-flight
  remote calls. A remote `tools/call` is run inside the same cancel-raced turn; the SDK client is
  dropped/aborted on shutdown.

### D9 — `session/load` does not persist server configs

`SessionStore` persists messages + model + cwd only. Zed re-sends `mcp_servers` on **every**
`session/load`/`session/resume`, so:

- **Case 1 (live in memory):** keep the existing bridge (already initialized on `session/new`).
- **Case 2 (rebuilt after restart):** build the bridge from the *request's* `mcp_servers`.
- **Case 3 (unknown):** unchanged — error, no bridge.

No server configuration is written to disk (avoids persisting secrets from stdio `env` / HTTP
`headers`).

### D10 — Analytics attribution

Native tools are recorded in the ops/MCP path; remote tools bypass ops, so the bridge records each
remote call directly into the same `AnalyticsStore` under its `mcp__{server}__{tool}` name, with
timing and `estimate_tokens` request/response token estimates. `session_stats` then attributes
remote usage alongside native tools. Recording is best-effort and never blocks the turn.

## Configuration

New `[acp.mcp]` section (validated in `config.rs`, documented in `daimonos.default.toml` and
`docs/configuration.md`; no magic numbers in code):

- `enabled` (bool, default `true`) — master switch for the bridge.
- `init_timeout_secs` (u64, default e.g. `10`) — per-server initialize+list-tools budget.
- `call_timeout_secs` (u64, default e.g. `60`) — per remote `tools/call` budget.
- `max_servers` (usize) and `max_tools_per_server` (usize) — bounds to keep the exposed tool set
  and spawned processes bounded (bounded-collections rule).
- `max_concurrent_connects` (usize, default `8`) — bounds simultaneous initialize/list handshakes;
  registration remains deterministic in forwarded order.
- `shared_pool_enabled` (bool, default `true`) — reuse identical initialized clients across ACP
  sessions; disable for intentionally session-stateful servers.
- (transport enables) `allow_stdio` / `allow_http` (bool, default `true`) — advertise + accept
  each transport.

## Component map (new + touched)

- **New `src/mcp_bridge.rs`** — `McpBridge` (per-session): builds clients from `Vec<McpServer>`,
  holds `name → (client, original_tool)` map + exposed `Vec<ToolSchema>`, dispatches
  `call(name, input)`, records analytics, and `shutdown()`s all clients. A minimal
  `McpClientHandler` impl (no-op server-initiated requests).
- **`src/agent.rs`** — add optional `remote_tool_dispatch` to `AgentConfig`; consult it in the
  `tool_facade::invoke → None` branch.
- **`src/acp_cmd.rs`** — read `req.mcp_servers` in `session/new` and the `session/load` rebuild
  path; build the bridge; append its schemas in `build_agent_config`; store bridge on
  `SessionHandle`; wire the dispatch hook; shut down on delete/exit; advertise `mcp` capability at
  initialize.
- **`src/config.rs` / `daimonos.default.toml` / `docs/configuration.md`** — `[acp.mcp]` config.
- **`Cargo.toml`** — enable `client`, `streamable-http`, `sse` features on `rust-mcp-sdk`.

## Testing (TDD)

- **`config.rs`** — `[acp.mcp]` parse + validation (defaults, zero-timeout rejection, bounds).
- **`mcp_bridge.rs`** — name namespacing/sanitize/truncate; native-wins and remote-vs-remote
  collision resolution; fail-open when a server can't start (skipped, others survive); dispatch maps
  namespaced → original tool name; shutdown reaps clients (lifecycle assertion). An in-process stub
  MCP server (spawned via the SDK server half, or a tiny stdio echo server) exercises a real
  `initialize`/`tools/list`/`tools/call` round-trip.
- **`acp_cmd.rs`** — a `session/new` carrying `mcp_servers` exposes `mcp__*` tools; a prompt that
  calls one round-trips through the bridge; a bad server does not fail `session/new`; native tools
  and lazy filtering are unchanged when `mcp_servers` is empty.
- **pytest** — the MCP-server-mode conformance suite must be unaffected (native tool set unchanged).
  ACP is not exercised by pytest today; Rust integration tests cover the ACP paths.

## Out of scope

- MCP resources/prompts/roots from remote servers (only `tools/*` is bridged here).
- Persisting remote server configs across restarts (Zed re-sends them; D9).
- The `unstable_mcp_over_acp` (`McpServer::Acp`) variant.
