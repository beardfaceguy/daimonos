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

This makes one-off headless overrides possible without rewriting a complete
temporary file:

```bash
DAIMONOS_AGENT_MODEL=anthropic/claude-opus-4.8 \
DAIMONOS_AGENT_APPROVAL_MODE=auto \
daimonos agent "review this change"
```

## Settings

### `[index]` — Workspace Indexing

Daimonos builds a trigram index of your workspace in the background for fast
search. These settings control what gets indexed.

| Setting | Default | Description |
|---------|---------|-------------|
| `max_depth` | `20` | Maximum directory traversal depth |
| `max_file_size` | `1000000` (1 MB) | Skip files larger than this (bytes) |
| `binary_sniff_bytes` | `512` | Bytes to check for null bytes when detecting binary files |
| `skip_extensions` | *(see below)* | File extensions to skip (known binary formats) |
| `max_files` | `50000` | Hard cap on indexed files; the walk stops at this count. Also the preflight budget for `guard_overbroad_roots`. `0` disables the cap |
| `guard_overbroad_roots` | `true` | Gate eager indexing on a signal: a root larger than the preflight budget is indexed only if it has a `project_markers` entry |
| `project_markers` | `.git`, `.hg`, `.svn`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Gemfile`, `pom.xml`, `build.gradle`, `CMakeLists.txt`, `Makefile` | Filenames marking a root as a real project for the gate |

`guard_overbroad_roots` replaces a brittle path blocklist with a property
check. daimonos runs a bounded preflight walk of the root:

- **Within `max_files`** (small) -> index it, whatever it is.
- **Larger than `max_files`** -> index it only if a `project_markers` entry
  is present at the root; otherwise serve with an **empty index**.

This stops daimonos from crawling an over-broad directory it inherited as cwd
(commonly `$HOME` — an editor window with no project open — but equally a NAS
mount or a downloads dir), which has been observed to balloon a single
instance to ~1.3 GB RSS and exhaust the inotify watch cap. The filesystem
root (`/`) is always skipped. File, exec, and read tools still work on a gated
root; supplying an explicit `-w` or completing an MCP `roots` handshake
re-roots the session to a real project and indexes it normally.

The default `skip_extensions` list covers images, audio, video, archives,
compiled objects, fonts, databases, and office documents. Directories named
`.git`, `node_modules`, and `target` are always skipped.

```toml
[index]
max_depth = 20
max_file_size = 1_000_000
binary_sniff_bytes = 512
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
max_servers = 32
max_concurrent_connects = 8
max_tools_per_server = 128
```

### `[logging]` — Runtime Diagnostics

Daimonos writes structured JSON logs to a rotating file independently of the
ACP/MCP transport. Protocol stdout is never used for logging. Warning and error
events are also mirrored to stderr so hosts such as Zed can surface failures.
Logs intentionally exclude prompts, API keys, MCP headers, tool arguments, and
file contents. They do retain operational metadata needed for diagnosis,
including absolute workspace paths, session IDs, MCP server names, and tool
names; protect the log directory accordingly.

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

**Large monorepos**: increase `max_depth` and `max_file_size` if important
files are deeply nested or large. Consider adding project-specific binary
extensions to `skip_extensions`.

**Slow indexing**: the index builds incrementally after the first full scan.
If the initial scan is slow, check that `skip_extensions` covers your binary
artifacts.

**Truncated exec output**: if you're losing important build output, increase
`exec_output_max_chars`. The default of 100 KB covers most cases.
