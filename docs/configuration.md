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
