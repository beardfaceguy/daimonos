# Configuration Reference

Daimonos uses a TOML config file for tuning indexing, search, and process
behavior. All settings have sensible defaults — configuration is optional.

## Config File Location

Daimonos searches for config in this order:

1. Path passed via `--config` / `-c` flag
2. `daimonos.toml` in the workspace root
3. `~/.config/daimonos/config.toml`
4. Built-in defaults (equivalent to `daimonos.default.toml` in the repo)

To start with a custom config, copy the reference file:

```bash
cp daimonos.default.toml ~/.config/daimonos/config.toml
```

Or place it in your project:

```bash
cp daimonos.default.toml /path/to/project/daimonos.toml
```

### Which config file is in use?

To see the discovery order and which file (if any) daimonos will load:

```bash
daimonos --print-config-path            # uses default workspace (.)
daimonos --print-config-path -w /path   # for a specific workspace
daimonos --print-config-path --config /path/to/file.toml
```

It prints each candidate in search order with a `[found]` / `[not found]`
marker and the file that wins (or `built-in defaults` if none exist), then
exits without starting the server. In non-MCP modes daimonos also logs
`config: loaded from …` to stderr at startup (add `--verbose` in `--mcp` mode).

## Agent Environment File

`daimonos agent`, `daimonos chat`, and `daimonos acp` use a separate,
dotenv-style `agent.env` for provider credentials and agent behavior. The file
is required; its location is selected in this order:

1. `--agent-env <path>`
2. `$DAIMONOS_AGENT_ENV`
3. `~/.config/daimonos/agent.env`

After reading the selected file, non-empty process variables named
`DAIMONOS_AGENT_*` override values from the file. Empty or whitespace-only
process values do not erase file values. Effective precedence is therefore:

```text
CLI behavior flags (for example --model) > process DAIMONOS_AGENT_* values
> selected agent.env file values
```

Required effective values remain
`DAIMONOS_AGENT_PROVIDER`, `DAIMONOS_AGENT_MODEL`,
`DAIMONOS_AGENT_BASE_URL`, `DAIMONOS_AGENT_APPROVAL_MODE`,
`DAIMONOS_AGENT_API_KEY`, and `DAIMONOS_AGENT_COMPACTION`. A required key may
come from either the file or the process environment; startup fails clearly
when it is absent from both. All existing provider, approval-mode, and
compaction validation applies after the override merge.

Supported providers are `anthropic`, `openrouter`, and native `openai`.
Native OpenAI uses the Responses API (not Chat Completions); a direct
GPT-5.6 Sol configuration is:

```dotenv
DAIMONOS_AGENT_PROVIDER=openai
DAIMONOS_AGENT_MODEL=gpt-5.6-sol
DAIMONOS_AGENT_BASE_URL=https://api.openai.com/v1
DAIMONOS_AGENT_API_KEY=sk-...
```

For `gpt-5.6-sol`, daimonos reports the documented 1,050,000-token context
window and 128,000-token maximum output capability. The model is text-only;
ACP image prompts are rejected before provider dispatch. OpenAI tool loops
retain encrypted reasoning continuation state locally with `store=false`.

This makes one-off headless overrides possible without rewriting a complete
temporary file:

```bash
DAIMONOS_AGENT_MODEL=anthropic/claude-opus-4.8 \
DAIMONOS_AGENT_APPROVAL_MODE=auto \
daimonos agent "review this change"
```

### Reasoning effort (`DAIMONOS_AGENT_THINKING`)

Optional. Sets the model's reasoning effort for both the one-shot
`daimonos agent` path and the `daimonos acp` prompt path. Valid values, in
ascending order: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`
(case-insensitive). Defaults to `medium` when the key is absent, so existing
`agent.env` files are unaffected. An unrecognized value fails startup with a
message naming the key and listing the valid levels.

```dotenv
DAIMONOS_AGENT_THINKING=high
```

How the level maps onto the wire request is provider-specific. On native
OpenAI the level is sent as `reasoning.effort`; note that its Responses
contract tops out at `xhigh`, so `max` is mapped conservatively onto the
same `xhigh` wire value — the two are therefore indistinguishable at that
provider (pick `xhigh` unless you specifically want `max` to mean the
maximum on a provider that exposes a distinct level). Provider defaults and
the Anthropic adaptive thinking behavior are unchanged by this key.

### Anthropic tool-prefix caching (`DAIMONOS_AGENT_PROMPT_CACHE`)

Optional and default-off. Set `DAIMONOS_AGENT_PROMPT_CACHE=on` to place an
ephemeral Anthropic prompt-cache breakpoint on the final tool definition,
caching the complete stable tool-schema prefix. Other providers ignore this
setting.

The first request pays Anthropic's cache-write premium. Repeated requests with
the same tools then use cheaper cache reads, so this favors tool-loop turns and
can cost more for a one-call response. A four-run Opus 4.8 Task 04 experiment
reduced fresh input 75.3% and cost 43.1% overall; at matched three-call behavior,
warm-cache cost fell approximately 54.7%. Keep it opt-in until the broader
native-agent suite confirms the one-call trade-off.

```dotenv
DAIMONOS_AGENT_PROMPT_CACHE=on
```

## Settings

### `[index]` — Workspace Indexing

Daimonos indexes relative file paths for `search` with `mode = "files"`.
Content search remains a direct filesystem grep and does not use this index.
Construction is O(1); population follows the configured rollout mode.

| Setting | Default | Description |
|---------|---------|-------------|
| `mode` | `"hybrid"` | `eager` always warms long-lived sessions, `lazy` waits for file search, and `hybrid` heuristically warms small or marked projects |
| `max_depth` | `20` | Maximum directory traversal depth |
| `skip_extensions` | *(see below)* | File extensions to skip (known binary formats) |
| `max_files` | `50000` | Hard cap on retained indexed paths. `0` uses an internal 50,000-file safety cap |
| `max_walk_entries` | `100000` | Hard cap on all filesystem entries visited by preflight and index walks; must be at least the effective `max_files` |
| `guard_overbroad_roots` | `true` | Gate eager indexing on a signal: a root larger than the preflight budget is indexed only if it has a `project_markers` entry |
| `project_markers` | `.git`, `.hg`, `.svn`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Gemfile`, `pom.xml`, `build.gradle`, `CMakeLists.txt`, `Makefile` | Filenames marking a root as a real project for the gate |

Hybrid mode uses `guard_overbroad_roots` as a bounded warm-start heuristic:

- **Within `max_files`** (small) -> index it, whatever it is.
- **Larger than `max_files`** -> warm only if a `project_markers` entry is
  present at the root.

This stops daimonos from crawling an over-broad directory it inherited as cwd
(commonly `$HOME` — an editor window with no project open — but equally a NAS
mount or a downloads dir), which has been observed to balloon a single
instance to ~1.3 GB RSS. A skipped warmup is not an empty-search failure:
the first file search performs a bounded population and reports `partial`
coverage when it reaches the cap. After population, a watcher marks the warm
index dirty so the next search refreshes external changes.

The default `skip_extensions` list covers images, audio, video, archives,
compiled objects, fonts, databases, and office documents. Directories named
`.git`, `node_modules`, and `target` are always skipped.

```toml
[index]
mode = "hybrid"
max_depth = 20
max_files = 50_000
max_walk_entries = 100_000
skip_extensions = [
  "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg",
  "mp3", "mp4", "avi", "mov", "mkv", "flac", "wav", "ogg", "webm",
  "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst",
  "exe", "dll", "so", "dylib", "o", "a", "lib",
  "wasm", "pyc", "pyo", "class",
  "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
  "sqlite", "db", "mdb",
  "ttf", "otf", "woff", "woff2", "eot",
]
```

### `[search]` — Search Limits

| Setting | Default | Description |
|---------|---------|-------------|
| `default_grep_max` | `100` | Max results for content search (grep) when not specified by caller |
| `default_find_max` | `20` | Max results for file search (trigram) when not specified |

```toml
[search]
default_grep_max = 100
default_find_max = 20
```

### `[process]` — Command Execution

| Setting | Default | Description |
|---------|---------|-------------|
| `poll_tail_lines` | `20` | Number of trailing output lines returned by `poll` for background processes |
| `exec_output_max_chars` | `100000` (100 KB) | Max characters of exec stdout/stderr before auto-truncation |
| `exec_stream_chunk_bytes` | `8192` (8 KB) | Read size for live foreground-exec updates sent to ACP clients |
| `extra_path` | *(none)* | Additional directories to prepend to `PATH` for exec/bg commands |
| `max_background_processes` | `16` | Maximum running or stopping background processes owned by one session |
| `termination_grace_ms` | `2000` | Grace between TERM and KILL for an owned process group |
| `output_memory_bytes` | `1048576` (1 MiB) | Maximum bytes retained in memory per stdout/stderr stream while reading |
| `artifact_max_bytes` | `104857600` (100 MiB) | Maximum bytes retained in one private background-output artifact |
| `artifact_directory` | `~/.daimonos/process-output` | Optional private process-artifact directory override |
| `default_timeout_secs` | `0` | Foreground/plugin deadline; `0` disables it |
| `inherit_env` | *(reviewed list)* | Exact ambient parent variables inherited before session/per-call overrides |
| `inherit_env_prefixes` | `LC_`, `XDG_` | Ambient parent prefixes inherited by managed children |

Managed execution bounds output while it is read, owns Unix process groups,
and retires them with TERM followed by KILL after the configured grace.
Background output uses random exclusive `0600` files under a `0700` directory;
the returned `log` field is the supported way to discover the path.

Auto-truncation keeps the first and last lines of output with a
`[N lines, M chars truncated]` notice in the middle. This prevents large build
outputs from consuming excessive tokens.

Common tool directories (`~/.cargo/bin`, `~/.local/bin`, etc.) are
auto-detected and added to `PATH`. Use `extra_path` for non-standard
locations:

```toml
[process]
poll_tail_lines = 20
exec_output_max_chars = 100_000
exec_stream_chunk_bytes = 8_192
extra_path = ["/opt/custom/bin", "/usr/local/go/bin"]
```

### `[tool_output]` — Model-visible tool results

Every text result returned to an agent or MCP client is bounded before it
enters model history. When a result exceeds either limit, Daimonos stores the
complete text in a private managed file and returns a UTF-8-safe head/tail
preview containing its path. Storage failures fail open: the successful tool
result remains unchanged.

Before each provider call, older successful tool results are also pruned
newest-first against a deterministic token budget. Errors and the configured
number of recent results are preserved. Large string arguments from old
`write_file` and `edit_file` calls are truncated without changing paths or
other metadata.

| Setting | Default | Description |
|---------|---------|-------------|
| `directory` | `~/.daimonos/tool-output` | Managed full-output directory; files are mode `0600` under a mode `0700` directory |
| `max_bytes` | `51200` | Maximum UTF-8 bytes in one model-visible text result |
| `max_lines` | `2000` | Maximum lines in one model-visible text result |
| `retention_days` | `7` | Delete older managed files during subsequent writes |
| `intra_turn_result_budget_tokens` | `40000` | Approximate newest-first budget for prior successful results |
| `intra_turn_keep_recent_results` | `5` | Most-recent successful results always preserved |
| `old_argument_max_chars` | `2000` | Threshold for truncating old edit/write string arguments |

```toml
[tool_output]
max_bytes = 51_200
max_lines = 2_000
retention_days = 7
intra_turn_result_budget_tokens = 40_000
intra_turn_keep_recent_results = 5
old_argument_max_chars = 2_000
```

### `[loop_detector]` — Tool retry-storm detection

Deterministic, LLM-free detection of tool retry storms inside a single agent
turn (vikunja #1197, adapted from Octomind). Each complete parallel batch of
tool calls plus results is one detector *round*. Every `(call, result)` pair is
fingerprinted; a round containing any novel pair counts as progress and resets
the windows, so legitimate repeated polls or staged reads never trigger it.
Uninterrupted identical repetition first injects a bounded corrective hint
(rotating sections from the `loop_steer` prompt) into the next request as
ephemeral system context. If the model changes its call-set, the detector
de-escalates; if it keeps repeating the exact call-set, reminders back off
exponentially until a hard circuit breaker ends the turn before paying for
another generation. Steers and breaker trips are recorded in analytics as
`context:loop_detector`.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Master switch; `false` removes the detector from the agent loop |
| `repeat_threshold` | `3` | Steer once an identical `(call, result)` pair repeats this many consecutive rounds; `0` disables |
| `no_novelty_rounds` | `3` | Steer after this many consecutive rounds with no novel pair; `0` disables |
| `circuit_breaker_rounds` | `12` | Hard-stop the turn after this many consecutive no-novelty rounds; `0` disables (steers only) |

```toml
[loop_detector]
enabled = true
repeat_threshold = 3
no_novelty_rounds = 3
circuit_breaker_rounds = 12
```

### `[mcp]` — MCP server (`--mcp`)

These settings apply only when running as an MCP server over stdio (Cursor,
Zed, etc.). Socket-mode daemon behavior is unchanged.

| Setting | Default | Description |
|---------|---------|-------------|
| `idle_timeout_secs` | `600` | Exit cleanly after this many seconds with **no** MCP requests (`list_tools`, `call_tool`, …). Protects against orphaned processes when an editor leaks stdin. Set `0` to disable. Overridable with `DAIMONOS_IDLE_TIMEOUT_SECS`. |
| `startup_logs` | `false` | When `false`, omit benign informational lines on stderr during MCP startup and idle shutdown (plugin registration, indexer stats, watchdog messages). Some MCP hosts (notably **Cursor**) classify **all** subprocess stderr as `[error]` in the UI. Use `--verbose`, `[mcp] startup_logs = true`, or `DAIMONOS_LOG_STARTUP=1` when debugging daimonos. |

```toml
[mcp]
idle_timeout_secs = 600
startup_logs = false
```

### `[session]` — Daemon-owned agent sessions

These limits apply to transport-independent agent sessions shared by ACP,
the local TUI/UDS client, and future remote clients.

| Setting | Default | Description |
|---------|---------|-------------|
| `socket_path` | workspace-derived | Optional local Unix socket override; by default a stable canonical-workspace hash allows distinct projects to run simultaneously. |
| `max_active_tool_calls` | `16` | Maximum in-flight tool calls tracked by one session. Must be greater than zero. |
| `max_sessions` | `64` | Maximum live sessions retained by one interactive session daemon. Must be greater than zero. |
| `max_clients_per_session` | `4` | Maximum clients simultaneously attached to one session. Must be greater than zero. |
| `event_queue_capacity` | `256` | Maximum canonical events queued per attached client. Must be greater than zero. |
| `snapshot_entries` | `2000` | Maximum transcript entries and tool calls retained independently in a full attach snapshot. Must be greater than zero. |
| `replay_events` | `512` | Maximum canonical session events retained for reconnect delta replay. |
| `approval_timeout_secs` | `30` | Maximum seconds a daemon approval may wait before it is denied safely. |
| `max_tool_event_output_bytes` | `65536` | Maximum UTF-8 bytes retained from one tool result in canonical events; range: 32 through one eighth of `max_frame_bytes`. |
| `accept_error_backoff_ms` | `100` | Delay before retrying a recoverable local Unix listener accept failure. |
| `idle_retention_secs` | `300` | Seconds before unloading a detached idle core while preserving its durable record for reattach; `0` keeps cores resident. |
| `session_list_page_size` | `50` | Maximum entries returned by one daemon session-list page. |
| `shutdown_grace_secs` | `5` | Maximum wait for daemon-owned prompt and client tasks during shutdown. |
| `remote_pairing_ttl_secs` | `300` | Lifetime of one single-use remote pairing claim. |
| `remote_pairing_wait_secs` | `300` | Maximum wait for local approval of a pairing request. |
| `remote_auth_timeout_secs` | `10` | Maximum time for the first remote authentication frame. |
| `remote_heartbeat_interval_secs` | `30` | Idle interval before sending a WebSocket ping. |
| `remote_heartbeat_timeout_secs` | `10` | Maximum wait for activity after a heartbeat ping. |
| `remote_max_messages_per_second` | `30` | Per-connection WebSocket text/control-frame rate limit. |
| `remote_max_connections` | `4` | Global concurrent remote WebSocket limit. |
| `remote_admission_attempts_per_minute` | `6` | WebSocket upgrade limit per source IP. |
| `remote_max_unauthenticated_per_ip` | `2` | Concurrent pre-authentication sockets allowed per source IP. |
| `remote_max_admission_peers` | `4096` | Maximum source addresses retained by remote admission accounting. |
| `remote_max_paired_devices` | `64` | Maximum paired devices retained during one daemon lifetime. Re-pairing replaces that device's previous ticket. |
| `max_frame_bytes` | `1048576` | Maximum newline-delimited JSON frame size. |
| `max_prompt_bytes` | `131072` | Maximum UTF-8 bytes in one prompt; its worst-case JSON-escaped event must fit within `max_frame_bytes`. |
| `max_label_bytes` | `256` | Maximum UTF-8 bytes in a client label. |
| `max_identifier_bytes` | `128` | Maximum UTF-8 bytes in session, client, request, approval, and option identifiers. |
| `max_ticket_bytes` | `1024` | Maximum authentication ticket bytes. |
| `max_runtime_value_bytes` | `4096` | Maximum UTF-8 bytes in a string runtime-option value. |
| `max_capabilities` | `16` | Maximum requested capabilities in one attach. |

```toml
[session]
# socket_path = "~/.daimonos/custom-session.sock"
max_active_tool_calls = 16
max_sessions = 64
max_clients_per_session = 4
event_queue_capacity = 256
snapshot_entries = 2000
replay_events = 512
approval_timeout_secs = 30
max_tool_event_output_bytes = 65536
accept_error_backoff_ms = 100
idle_retention_secs = 0
session_list_page_size = 50
shutdown_grace_secs = 5
remote_pairing_ttl_secs = 300
remote_pairing_wait_secs = 300
remote_auth_timeout_secs = 10
remote_heartbeat_interval_secs = 30
remote_heartbeat_timeout_secs = 10
remote_max_messages_per_second = 30
remote_max_connections = 4
remote_admission_attempts_per_minute = 6
remote_max_unauthenticated_per_ip = 2
remote_max_admission_peers = 4096
remote_max_paired_devices = 64
max_frame_bytes = 1048576
max_prompt_bytes = 131072
max_label_bytes = 256
max_identifier_bytes = 128
max_ticket_bytes = 1024
max_runtime_value_bytes = 4096
max_capabilities = 16
```

### `[tui]` — Interactive terminal frontend

These limits apply to `daimonos agent --interactive`. History is process-local
and is not persisted across launches.

| Setting | Default | Description |
|---------|---------|-------------|
| `history_entries` | `100` | Maximum submitted prompts retained for Up/Down navigation. Valid range: 1–10,000. |
| `scrollback_entries` | `2000` | Maximum entries retained independently for transcript lines and tool cards. Valid range: 1–50,000. |

```toml
[tui]
history_entries = 100
scrollback_entries = 2000
```

### `[acp]` — Native agent protocol (`acp`)

These settings apply to native ACP integrations such as Zed.

| Setting | Default | Description |
|---------|---------|-------------|
| `session_list_page_size` | `50` | Maximum saved sessions returned by one `session/list` response. When more remain, the response includes an opaque continuation cursor. Must be greater than zero. |

```toml
[acp]
session_list_page_size = 50
```

#### `[acp.mcp]` — MCP-server bridge

Zed forwards every configured context server to the ACP agent on `session/new`
and `session/load`. When the bridge is enabled, daimonos connects to each as an
MCP client, discovers its tools, and exposes them to the model as
`mcp__{server}__{tool}` alongside the native daimonos tools (ADR-003, #990).
Native tools always win a name collision. A server that fails to start,
connect, or list tools is skipped (fail-open) — the session still gets the
remaining servers and all native tools. Remote tool calls flow through the same
permission hooks as native destructive tools and are attributed in analytics
under their `mcp__…` name.

By default, identical transport configurations are initialized once and leased
from a process-wide pool across ACP sessions. Tool names/routes and analytics
attribution remain per session; the last lease shuts the client down. Disable
pooling for context servers whose internal state must be isolated per chat.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Master switch. When `false`, forwarded `mcp_servers` are ignored and the `mcp` agent capability is not advertised. |
| `allow_stdio` | `true` | Accept + advertise stdio-transport servers (`command`/`args`/`env`). |
| `allow_http` | `true` | Accept + advertise HTTP-transport servers (`url`/`headers`). |
| `shared_pool_enabled` | `true` | Reuse identical initialized clients across ACP sessions in one process. Set `false` for servers requiring per-chat state isolation. |
| `init_timeout_secs` | `10` | Per-server budget for the `initialize` + `tools/list` handshake. Exceeding it skips that server. Must be > 0 when enabled. |
| `call_timeout_secs` | `60` | Per remote `tools/call` budget. On timeout the model gets an error tool result and the turn continues. Must be > 0 when enabled. |
| `shutdown_timeout_secs` | `5` | Maximum MCP runtime/child shutdown wait. Expiry is logged and ACP teardown continues. Must be > 0 when enabled. |
| `teardown_timeout_secs` | `10` | Overall budget for concurrently shutting down every session bridge after ACP transport loss. Must be > 0 when enabled. |
| `max_servers` | `32` | Upper bound on forwarded servers connected per session. Must be > 0 when enabled. |
| `max_concurrent_connects` | `8` | Maximum server handshakes in flight at once. Results are registered in forwarded order for deterministic tool names. Must be > 0 when enabled. |
| `max_tools_per_server` | `128` | Upper bound on tools registered from any single server. Must be > 0 when enabled. |

```toml
[acp.mcp]
enabled = true
allow_stdio = true
allow_http = true
shared_pool_enabled = true
init_timeout_secs = 10
call_timeout_secs = 60
shutdown_timeout_secs = 5
teardown_timeout_secs = 10
max_servers = 32
max_concurrent_connects = 8
max_tools_per_server = 128
```

### `[logging]` — Runtime Diagnostics

Daimonos writes structured JSON logs to a rotating file independently of the
ACP/MCP transport. Protocol stdout is never used for logging. Warning and error
events are also mirrored to stderr so hosts such as Zed can surface failures.
Both outputs allow only tracing targets owned by Daimonos; dependency events
and raw MCP child-process stderr are dropped because they may contain secrets.
The log directory is restricted to mode `0700` and matching log files to
`0600` on Unix.

Daimonos-owned events intentionally exclude prompts, API keys, MCP headers,
tool arguments, and file contents. They do retain operational metadata needed
for diagnosis, including absolute workspace paths, session IDs, MCP server
names, and tool names.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Enables persistent structured logging. Initialization failure is fail-open and reported on stderr. |
| `level` | `info` | File filter: `trace`, `debug`, `info`, `warn`, `error`, or `off`. |
| `stderr_level` | `warn` | Stderr filter. ACP/MCP stdout remains protocol-only. |
| `directory` | `$XDG_STATE_HOME/daimonos/logs` or `~/.local/state/daimonos/logs` | Rotated log directory. `~` is expanded. |
| `file_prefix` | `daimonos` | Rotated filename prefix. |
| `rotation` | `daily` | `hourly`, `daily`, or `never`. |
| `max_files` | `14` | Maximum retained rotated files. Must be greater than zero. |
| `resource_interval_secs` | `15` | Process telemetry interval. Set to `0` to disable. |

```toml
[logging]
enabled = true
level = "info"
stderr_level = "warn"
# directory = "~/.local/state/daimonos/logs"
file_prefix = "daimonos"
rotation = "daily"
max_files = 14
resource_interval_secs = 15
```

Each process emits lifecycle, ACP session, workspace-index, MCP bridge, remote
tool timing, and shutdown events. `daimonos::telemetry` snapshots include PID,
uptime, RSS, thread count, open file descriptors, cumulative CPU scheduler
ticks, and interval CPU-tick deltas. To investigate a hot loop, temporarily set
`level = "debug"` and follow the newest file in the configured directory.

### `[observability]` — OTLP/Langfuse Tracing

Optional OpenTelemetry export provides a causal trace view of agent prompts,
LLM generations, tools, and context management (ADR-006). It is disabled by
default and does not replace local SQLite analytics or secure process logs.
Export uses a bounded background queue: a full queue drops spans rather than
blocking an agent turn, and initialization/export/shutdown failures are
fail-open.

Only tracing spans with the dedicated `daimonos::observability` target may
cross the OTLP boundary. Existing diagnostic events can contain local paths and
are explicitly excluded. Prompt, source, command, tool payload, model output,
thinking, headers, and credentials are not exported under defaults.

See **[observability.md](observability.md)** for the operational runbook: cloud
and self-host setup, the smoke test, credential rotation, sampling, retention,
overhead budgets, troubleshooting, disable/rollback, and comparing models and
token-saving strategies.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `false` | Enables OTLP trace export. Missing credentials disable export without failing the runtime. |
| `endpoint` | `http://localhost:3000/api/public/otel/v1/traces` | Exact OTLP/HTTP traces endpoint. Langfuse Cloud endpoints are region-specific. |
| `basic_auth` | `true` | Sends Basic Auth credentials. Set false for an unauthenticated local collector. |
| `basic_auth_username_env` | `LANGFUSE_PUBLIC_KEY` | Environment-variable name containing the Basic Auth username/public key. |
| `basic_auth_password_env` | `LANGFUSE_SECRET_KEY` | Environment-variable name containing the Basic Auth password/secret key. |
| `environment` | `development` | Deployment environment resource label. |
| `release` | unset | Optional release identifier. |
| `sample_ratio` | `1.0` | Parent-based sampling ratio from `0.0` through `1.0`. |
| `max_queue_size` | `2048` | Maximum completed spans awaiting export. |
| `max_batch_size` | `512` | Maximum spans per request; cannot exceed queue size. |
| `batch_delay_ms` | `5000` | Maximum delay before exporting a partial batch. |
| `flush_timeout_ms` | `3000` | Bounded shutdown/flush timeout. |

```toml
[observability]
enabled = false
endpoint = "http://localhost:3000/api/public/otel/v1/traces"
basic_auth = true
basic_auth_username_env = "LANGFUSE_PUBLIC_KEY"
basic_auth_password_env = "LANGFUSE_SECRET_KEY"
environment = "development"
# release = "v0.1.1"
sample_ratio = 1.0
max_queue_size = 2048
max_batch_size = 512
batch_delay_ms = 5000
flush_timeout_ms = 3000
```

Credential values are read only from the named environment variables and are
never included in configuration dumps or initialization errors. Langfuse's
generic OTLP base endpoint ends in `/api/public/otel`; because Daimonos
configures the signal-specific traces exporter directly, its endpoint includes
the required `/v1/traces` suffix.
Content capture is not currently configurable: export is unconditionally
metadata-only. A future redacted content mode will add an explicit opt-in only
when its privacy tests and limits ship with it.

### `[analytics]` — Token & Latency Tracking

Daimonos records every MCP tool call (request/response token estimates,
execution time, redirect/filter/dedup signals) to a SQLite database for
cross-session reporting. Disable to opt out entirely.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `true` | Master switch. When `false`, nothing is tracked and the SQLite file is never opened. |
| `db_path` | `~/.daimonos/analytics.db` | Path to the SQLite store. Created on first run with WAL mode. |
| `retention_days` | `90` | Records older than this are auto-pruned roughly every 100 inserts. |

```toml
[analytics]
enabled = true
# db_path = "~/.daimonos/analytics.db"
retention_days = 90
```

The data is exposed three ways:

- **`session_stats` MCP tool** — query `session`, `history`, or `daily`
  scope. The `session` scope echoes the live `external_session_id` so
  the agent can confirm correlation is wired up. `history` and `daily`
  accept an optional `external_session_id` argument that filters the
  result set to a single agent-runtime session.
- **`workspace_info` MCP tool** — includes a compact `analytics`
  summary block (calls / req_tokens / resp_tokens / saved_tokens /
  redirects / filters / dedup_hits / db_path / external_session_id).
- **`daimonos --stats`** — human-readable report. Pass
  `--session-id <id>` to filter the history/daily blocks to a single
  agent-runtime session id (useful with
  `claude --session-id $SID` / `DAIMONOS_AGENT_SESSION_ID=$SID`).

### Correlating with agent-runtime sessions

Daimonos's analytics rows include an optional `external_session_id`
column so per-tool-call tokens can be joined post-hoc with the agent's
own usage logs. Two ways to attach the id:

1. **Bootstrap from environment** — set `DAIMONOS_AGENT_SESSION_ID`
   before launching daimonos. The MCP server reads it once at startup
   and stamps every analytics row in that connection with that id.
   Most useful when you control the launch (CI, benchmark harnesses,
   `claude --session-id $SID`).
2. **Mid-session via MCP tool** — call
   `set_external_session_id({"id": "<sid>"})`. Subsequent rows pick up
   the new id. Pass `""` to clear. Useful when the agent is launched
   by an editor (Cursor, Claude Code Desktop) and the user can't set
   environment variables on the daimonos subprocess.

```bash
# Bench-harness pattern: same UUID on both sides.
SID=$(uuidgen)
DAIMONOS_AGENT_SESSION_ID=$SID claude --session-id "$SID" \
    --mcp-config .cursor/mcp.json --strict-mcp-config \
    --tools "" "Refactor the login flow"

# Later, query just this session's daimonos-side cost:
daimonos --stats --session-id "$SID"
```

### `[discord]` — Discord Integration Foundation

These settings define bot-token auth, allowlists, and token-budget limits for
Discord integration. Phase 1 defaults are conservative: integration disabled,
deny-by-default allowlists, and read-only behavior.

| Setting | Default | Description |
|---------|---------|-------------|
| `enabled` | `false` | Enables Discord integration behavior and startup validation |
| `bot_token_env_var` | `"DISCORD_BOT_TOKEN"` | Env var name used to load the bot token |
| `api_base_url` | `"https://discord.com/api/v10"` | Discord REST API base URL |
| `allow_guild_ids` | `[]` | Explicitly allowed guild IDs (Discord snowflakes) |
| `allow_channel_ids` | `[]` | Explicitly allowed channel IDs (Discord snowflakes) |
| `max_messages_per_call` | `100` | Max messages returned by a single read/search call |
| `max_message_chars` | `4000` | Per-message text truncation cap |
| `max_response_chars` | `32000` | Total response payload cap for Discord tool output |
| `read_only_default` | `true` | Keeps write-style actions disabled by default |
| `rate_limit_max_retries` | `2` | Max retry attempts when Discord responds with HTTP 429 |
| `rate_limit_max_sleep_ms` | `10000` | Max per-retry sleep duration (ms) when honoring retry hints |

```toml
[discord]
enabled = false
bot_token_env_var = "DISCORD_BOT_TOKEN"
api_base_url = "https://discord.com/api/v10"
allow_guild_ids = []
allow_channel_ids = []
max_messages_per_call = 100
max_message_chars = 4_000
max_response_chars = 32_000
read_only_default = true
rate_limit_max_retries = 2
rate_limit_max_sleep_ms = 10_000
```

### `[kgl]` — Knowledge-Graph Layer

Tunables for the KGL code+intent knowledge graph. KGL is itself gated on the
`DAIMONOS_KGL_AUTOINDEX` / `DAIMONOS_KGL_OBSERVE` environment variables; these
values govern its SQLite access and background file-watcher when enabled.

| Setting | Default | Description |
|---------|---------|-------------|
| `busy_timeout_ms` | `5000` | SQLite busy-timeout (ms) on every KGL store connection. With WAL journaling (enabled automatically) this lets the watcher's writer and the `kgl_query`/`kgl_assert` readers wait briefly for a lock instead of erroring with `SQLITE_BUSY`. |
| `max_watches` | `4096` | Hard cap on inotify watches the KGL file-watcher registers, bounding `fs.inotify.max_user_watches` usage. |
| `debounce_secs` | `2` | Debounce window for coalescing change bursts into at most one graph rebuild. |
| `orient_max_matches` | `12` | Max task-matching defs `kgl_query orient` expands in one bundled call (each adds its edges + dependents), bounding the orient response size. |
| `find_max` | `200` | SQL LIMIT applied to every `kgl_query find` result set; caps how many nodes a broad LIKE query can materialise in one call. |
| `blast_radius_max` | `500` | Hard node cap for `blast_radius` BFS; stops dense call graphs from exhausting CPU/memory during a transitive traversal. |
| `skip_dirs` | `["target", ".git", ".jj", "node_modules", ".kgl", "graphify-out"]` | Directory base names never walked when detecting/indexing a substrate or registering watches. |

```toml
[kgl]
busy_timeout_ms = 5000
max_watches = 4096
debounce_secs = 2
orient_max_matches = 12
find_max = 200
blast_radius_max = 500
skip_dirs = ["target", ".git", ".jj", "node_modules", ".kgl", "graphify-out"]
```

### `[prompts]` — Model-Facing Prompt Overrides

The text that steers daimonos's behavior is embedded in the binary as defaults
but can be overridden at runtime without recompiling. Prompt-file contents
**replace** their built-in defaults. The tool-description catalog is a partial
overlay: omitted tools and variants keep their embedded values. Unset/empty keys
use the embedded default; unreadable files warn and fall back. `~` expands to
`$HOME`. Prompt files are sent to the model verbatim, so do **not** put comments
inside them. See `prompts/README.md` for the committed defaults and guidance.

```toml
[prompts]
# agent_system     = "~/.config/daimonos/prompts/agent_system.md"
# mcp_instructions = "~/.config/daimonos/prompts/mcp_instructions.md"
# kgl_hint         = "~/.config/daimonos/prompts/kgl_hint.md"
# summary          = "~/.config/daimonos/prompts/summary.md"
# tool_descriptions = "~/.config/daimonos/prompts/tool_descriptions.toml"
```

| Key | Used by | Purpose |
|-----|---------|---------|
| `agent_system` | `daimonos agent` / `chat` / ACP | Core agent system prompt (tool-use strategy, `execute_script` preference). |
| `mcp_instructions` | `daimonos --mcp` | Server instructions sent to the MCP host, including the terse-output directive that affects output token cost. |
| `kgl_hint` | `daimonos --mcp` (KGL auto-index only) | Nudge to orient via the knowledge graph before reading source. |
| `summary` | context compaction | System prompt for the summarizer that replaces evicted turns. |
| `tool_descriptions` | MCP / `agent` / `chat` / ACP | Partial TOML overlay for full/terse tool descriptions and nested `[tool.parameters]` JSON Schema property descriptions. |

**Getting the baseline defaults**: the defaults are embedded in the binary, so
you don't need the source to see or copy them:

```bash
daimonos --print-prompt mcp_instructions      # print one default to stdout
daimonos --dump-prompts                        # scaffold all five resources into
                                               #   ~/.config/daimonos/prompts/
daimonos --dump-prompts /path/to/dir           # ...into a custom directory
daimonos --dump-prompts --force                # overwrite existing files
```

`--dump-prompts` writes the four `<name>.md` prompts and
`tool_descriptions.toml` (skipping existing files unless `--force`), then prints
a ready-to-paste `[prompts]` block. Start from these so an override begins at —
and can be diffed against — the baseline.

**Additional agent instructions**: `daimonos agent`, `daimonos chat`, and ACP
append `~/.config/daimonos/agent-instructions.md` to the resolved
`agent_system` prompt when that file exists. If `$XDG_CONFIG_HOME` is set, it
replaces `~/.config`. Override the file for a run with the global flag:

```bash
daimonos agent "task" --agent-instructions /path/to/rules.md
daimonos chat --agent-instructions /path/to/rules.md
daimonos acp --agent-instructions /path/to/rules.md
```

The additional file is appended verbatim with a blank-line separator. A missing
default file is silently ignored. An explicit unreadable override — or a default
file that exists but cannot be read — is an error, preventing configured rules
from being silently omitted. This does not apply to `daimonos --mcp`.

**Warning**: these change how the agent behaves. Removing the `execute_script`
guidance or the terse directive typically **increases** token usage. Override
deliberately and prefer editing copies over the committed defaults so you can
compare against the baseline.

`summary` is also overridable via the `DAIMONOS_AGENT_SUMMARY_PROMPT` agent-env
variable, which takes precedence over `[prompts].summary`. Precedence:
`DAIMONOS_AGENT_SUMMARY_PROMPT` > `[prompts].summary` > embedded `summary.md`.

### `[tools.<id>]` — Tool Plugins (Advanced)

Register external tools for the tool runner system. Most users don't need
this — the built-in plugins for git, cargo, gh, and docker are auto-registered
when the binaries are found on `PATH`.

```toml
[tools.x07]
bin = "/path/to/x07"
source_pattern = "**/*.x07.json"
manifest = "x07.json"
```

## Environment Variables

These are not part of the config file but affect daimonos behavior:

| Variable | Description |
|----------|-------------|
| `DAIMONOS_IDLE_TIMEOUT_SECS` | Overrides `[mcp] idle_timeout_secs` when set to a parseable integer. `0` disables the idle watchdog. Used by tests. |
| `DAIMONOS_LOG_STARTUP` | When non-empty and not `0` / `false` / `no`, enables MCP stderr startup diagnostics before config load (same family as `--verbose` / `[mcp] startup_logs`). |
| `DAIMONOS_AGENT_SESSION_ID` | Optional agent-runtime session id (e.g. a UUID matching `claude --session-id`). When set, every analytics row recorded by this daimonos process is tagged with this id, and `daimonos --stats --session-id <id>` / `session_stats {external_session_id: <id>}` will filter to it. The `set_external_session_id` MCP tool can override this at runtime. |
| `DAIMONOS_AGENT_SUMMARY_PROMPT` | Overrides the context-compaction summarizer prompt. Takes precedence over `[prompts].summary` and the embedded default. |
| `DISCORD_BOT_TOKEN` | Default Discord bot token variable when `[discord].bot_token_env_var` is unchanged. |
| `PATH` | Daimonos inherits the launching process's `PATH` for exec/bg commands |

## Performance Tuning Tips

**Large monorepos**: increase `max_depth`, `max_files`, and
`max_walk_entries` together if important paths fall outside reported
coverage. Add project-specific generated/binary extensions to
`skip_extensions` to spend the path budget on useful files.

**Slow first file search**: use `mode = "hybrid"` or `"eager"` to warm
long-lived sessions, or narrow `path` and `glob` filters. Later searches reuse
the warm path index until its watcher reports a filesystem change.

**Truncated exec output**: if you're losing important build output, increase
`exec_output_max_chars`. The default of 100 KB covers most cases.
