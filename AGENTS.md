# AGENTS.md

Cross-tool guidance for AI coding agents working in this repository.

## Quick start (new agents)

**Do these in order. Do NOT skip to reading source code.**

1. Read this file fully — project context, repo layout, conventions.
2. **Go to Linear first.** Use the Linear MCP tools to read the Daimonos
   initiative (`get_initiative` with query "Daimonos"), list all issues in the
   daimonos project (`list_issues` with project "daimonos"), and list project
   documents (`list_documents` with project "daimonos"). This is the
   authoritative source for what's been done, what's in progress, and what's
   planned. Understand the full project state from Linear before touching
   the codebase.
3. Check `.cursor/rules/` if scoped rule files exist.
4. Check `.cursor/strategies/` if scoped strategy/pattern files exist.
4. Check `docs/` for technical specs (protocol, architecture decisions).
5. **Only then** read source code as needed for your specific task.

## What is Daimonos?

Daimonos is a **bare-metal operating system purpose-built for AI agents**. The
kernel, filesystem, process model, and syscall interface are designed from the
ground up for agent workloads — no shell, no terminal, no human-facing
interfaces.

The name comes from Greek *daimon* (agent/spirit), the etymological root of
"daemon."

**The current codebase is Phase 1: a user-space protocol prototype** running as
a daemon on Linux. It proves out the opcode protocol and structured I/O design
before they become native syscall interfaces in the eventual bare-metal OS.

For the full vision (kernel, StructFS, capability security, phased roadmap),
read the Daimonos initiative description in Linear.

## Systems of record

| What | Where |
|------|-------|
| Vision, roadmap, project status | Linear initiative "Daimonos" |
| Research findings and analysis | Linear project documents |
| Task status and ownership | Linear issues |
| Install & setup guides | `docs/install.md` + per-tool guides in `docs/` |
| Protocol specification | `docs/protocol.md` |
| Configuration reference | `docs/configuration.md` and `daimonos.default.toml` |
| Architecture decisions | `docs/` directory |
| Coding rules and conventions | This file + `.cursor/rules/` |

Technical documentation that doesn't fit in Linear (specs, diagrams,
benchmarks, design docs) goes in `docs/`. Use Linear documents for research
findings, status reports, and narrative context. Use `docs/` for anything an
agent or developer needs while reading or writing code.

## Repo layout

```
daimonos/
├── README.md                      # Project overview for humans
├── AGENTS.md                      # This file (agent guidance)
├── Cargo.toml                     # Rust project manifest
├── daimonos.default.toml          # Reference config with all tunable values
├── server.json                    # GitHub MCP Registry metadata
├── mcpb/
│   └── manifest.json              # MCPB bundle manifest template (binary server)
├── benchmarks/
│   ├── README.md                  # How to run benchmarks
│   ├── run-benchmark.sh           # Runner: executes tasks via agent CLI
│   ├── setup-mcp.sh               # Configures MCP for daimonos mode
│   ├── analyze-results.py         # Compares cursor vs daimonos results (single model)
│   ├── compare-models.py          # Cross-model comparison report
│   ├── remote/                    # AWS remote benchmark orchestration
│   │   ├── run-remote-benchmark.sh    # Launch, provision, run, collect, teardown
│   │   ├── provision-ubuntu.sh        # Provisions Ubuntu baseline instance
│   │   ├── provision-daimonos.sh      # Provisions daimonos distro instance
│   │   └── collect-results.sh         # Standalone result collector
│   ├── tasks/                     # Task definitions (JSON)
│   └── workspace/                 # Target codebase (Rust inventory app)
├── distro/
│   ├── build-buildroot.sh         # Build script: Buildroot disk image (canonical)
│   ├── test-qemu.sh               # Boot image in QEMU for local testing
│   ├── deploy-aws.sh              # Deploy image to AWS (S3 + AMI)
│   ├── br2-external/              # Buildroot external tree (defconfig, overlay, board)
│   └── alpine-legacy/             # Deprecated Alpine build (reference only)
├── docs/
│   ├── install.md                 # Build & install instructions
│   ├── cursor-setup.md            # Cursor IDE integration guide
│   ├── copilot-setup.md           # GitHub Copilot (VS Code, Visual Studio, JetBrains, Xcode, Eclipse)
│   ├── claude-code-setup.md       # Claude Code integration (CLI + macOS Desktop)
│   ├── windsurf-setup.md          # Windsurf IDE integration guide
│   ├── cline-setup.md             # Cline (VS Code extension) integration guide
│   ├── gemini-cli-setup.md        # Gemini CLI integration guide
│   ├── zed-setup.md               # Zed editor integration guide
│   ├── other-tools-setup.md       # General MCP setup (Claude Desktop, ChatGPT, Continue.dev, etc.)
│   ├── configuration.md           # Config file reference (all tunables)
│   └── protocol.md                # Opcode protocol specification
├── tests/
│   ├── conftest.py                # pytest fixture: DaimonosClient + MCP handshake
│   ├── requirements.txt           # pytest
│   ├── test_handshake.py          # MCP initialize, tool listing
│   ├── test_read_write.py         # read_file / write_file
│   ├── test_edit.py               # edit_file
│   ├── test_search.py             # search (content + file modes)
│   ├── test_exec.py               # exec
│   ├── test_workspace_info.py     # workspace_info
│   ├── test_errors.py             # Missing args, unknown tools, invalid paths
│   ├── test_symlinks.py           # Symlink and hard link handling across all file ops
│   ├── test_diff.py               # diff_files tool
│   ├── test_git.py                # unified git tool (status, log, diff, branch, add, commit, etc.)
│   ├── test_snapshots.py          # unified snapshot tool (create, restore, list, delete)
│   ├── test_batch.py             # batch tool (multi-op single round-trip)
│   └── test_analytics.py         # session_stats tool, workspace_info analytics, dedup tracking
└── src/
    ├── main.rs                    # Entrypoint: --mcp (stdio MCP), Unix socket, or --stats
    ├── analytics.rs               # Token analytics: SQLite-backed per-tool-call tracking
    ├── mcp.rs                     # MCP server handler (stdio transport for Cursor)
    ├── config.rs                  # TOML config loading, tool registration
    ├── protocol.rs                # Request/Response types, opcode constants
    ├── session.rs                 # Per-connection session state (exposed tools, used tools)
    ├── tools.rs                   # ToolDef registry: single source of truth for all tool definitions
    ├── script.rs                  # Starlark interpreter: execute_script tool, tool function bindings
    ├── snapshot.rs                # Workspace snapshot store (create, restore, list, delete)
    ├── index.rs                   # Background trigram workspace indexer
    ├── pipeline_cache.rs          # inotify-based tool output cache
    ├── tool_runner.rs             # ToolPlugin trait, registry, repair loop
    ├── ops/
    │   ├── mod.rs                 # Opcode dispatcher
    │   ├── file_ops.rs            # Opcodes 0-6: read, write, patch, ls (auto-skips .git/node_modules/target), stat, glob, grep
    │   ├── exec_ops.rs            # Opcodes 8-11: exec, bg, poll, kill
    │   ├── exec_filter.rs         # Semantic exec output filters (test, build, install, lint)
    │   ├── diff_ops.rs            # Opcode 14: diff (in-process structured diffing)
    │   ├── snap_ops.rs            # Opcodes 12-13, 25-26: snap, restore, snap_list, snap_delete
    │   ├── tool_ops.rs            # Opcodes 20-24: tool_run, repair, pipeline, register, list
    │   └── schema.rs              # Opcode 255: self-describing schema registry
    └── plugins/
        ├── mod.rs                 # Plugin module
        ├── generic_cli.rs         # Generic plugin for any CLI tool with JSON output
        ├── git.rs                 # Git tool plugin (auto-registered when git on PATH)
        ├── cargo.rs               # Cargo plugin: test, build, check, clippy, fmt, add (auto-registered)
        ├── gh.rs                  # GitHub CLI plugin: pr_view/list/create/diff/checks, api (auto-registered)
        ├── docker.rs              # Docker plugin: ps, logs, exec, images, inspect, stop, compose (auto-registered)
        └── x07.rs                 # X07-specific plugin (semantic indexing, decl cache)
```

## Prerequisites

| Dependency | Required by | Install |
|-----------|-------------|---------|
| Rust (stable) | Building daimonos | `rustup` |
| `socat` | Manual testing via Unix socket | system package manager |
| Python 3 + pytest | MCP protocol tests | `pip install -r tests/requirements.txt` |

## Coding conventions

- **Language**: Rust, edition 2021
- **Async runtime**: Tokio
- **Serialization**: serde + serde_json for protocol, toml for config
- **No hardcoded values**: all tunables go in `daimonos.default.toml` and
  `config.rs`. Never hardcode file extensions, limits, or defaults in
  operational code.
- **Compact field names**: protocol fields use single-letter keys (`c`, `p`,
  `s`, `n`, `a`, `g`, `kv`) to minimize token cost. Response fields are
  similarly terse (`d` for data, `e` for error code, `m` for message).
- **Structured output only**: every response is JSON. Never return unstructured
  text. If a subprocess produces text, wrap it in `{"raw": "..."}`.
- **Error handling**: use `Response::err(code, msg)` with numeric error codes
  (1=not found, 2=permission, 3=invalid arg, 4=IO, 5=process, 6=timeout,
  7=snapshot not found). No panics in request handlers.
- **New opcodes**: assign the next available number, add to `protocol::op`,
  add handler in `ops/mod.rs`, add schema entry in `ops/schema.rs`.
- **Opcodes vs. tool plugins vs. exec**: opcodes are reserved for core OS
  primitives with no external binary dependencies (read, write, diff, etc.).
  External tools (git, cargo, npm, docker) belong in the **tool plugin
  system** (`plugins/`) — implement `ToolPlugin`, auto-register at startup
  when the tool is detected on PATH. For one-off commands, agents use `exec`
  directly. Never add an opcode for an external binary.
- **Exec usage tracking**: all `exec` and `bg` invocations are counted in
  `session.exec_usage`. Use `session_info` to see the top commands. When a
  tool shows up frequently, consider adding a declarative tool descriptor.
- **Read deduplication**: `session.read_cache` tracks content hashes of files
  the model has read. On re-read of an unchanged file, the response is
  `{"unchanged": true, "lines": N}` instead of the full content. `write` and
  `patch` operations invalidate the cache for the affected path. Partial
  reads (with offset/limit) bypass the cache entirely.
- **Edit confirmation with diffs**: `edit_file` (patch opcode) returns a
  `diffs` array alongside `applied`, showing each `[old, new]` pair that
  was successfully applied. When nothing matches, `diffs` is omitted.
- **Exec output filtering**: when `config.process.exec_output_filters` is
  enabled (default: true), recognized commands get semantic output
  compression via `ops/exec_filter.rs`. Test runners (cargo test, pytest,
  jest, go test) return only summary + failures. Build commands (cargo
  build, make, go build) return "ok" on success or just error/warning
  lines on failure. Install commands (pip install, npm install) return
  "ok: install complete" or error-only output. Linters get the same
  treatment as builds. Unknown commands pass through unfiltered.
- **Exec output capping**: `exec` output (stdout and stderr) is
  auto-truncated when it exceeds `config.process.exec_output_max_chars`
  (default 100 KB). Truncated output keeps the first and last lines with a
  `[N lines, M chars truncated]` notice in the middle. Capping is applied
  after filtering as a safety net.
- **Lazy tool exposure**: only core tools (`read_file`, `write_file`,
  `edit_file`, `search`, `workspace_info`, `exec`, `batch`,
  `list_all_tools`) plus commonly-used consolidated tools (`git`,
  `cargo`, `gh`, `docker`, `snapshot`, `set_cwd`, `ls`) appear in the
  initial `list_tools` response. Context-aware tools (`git`, `cargo`,
  `gh`) are hidden when their prerequisites are missing (no `.git` or
  `Cargo.toml`). Extended tools (`diff_files`, `tool_pipeline`,
  `tool_repair`) are exposed after the model calls `list_all_tools` or
  uses one directly. The set of exposed tools is tracked in
  `session.exposed_tools`.
- **Proactive workspace context**: the MCP `instructions` field is built
  dynamically at startup with workspace path, detected project type
  (Cargo.toml → Rust, package.json → Node.js, etc.), VCS info, and
  top-level directory listing — so the model has useful context without an
  extra tool call.
- **Consolidated tools**: related operations are grouped into single MCP
  tools with an action/command parameter to minimize schema overhead.
  `git` has a `command` enum (status, log, diff, branch, add, commit,
  push, pull, checkout). `cargo` has a `command` enum (test, build,
  check, clippy, fmt, add). `gh` has a `command` enum (pr_view, pr_list,
  pr_create, pr_diff, pr_checks, api). `docker` has a `command` enum
  (ps, logs, exec, images, inspect, stop, compose_up, compose_down,
  compose_ps). `snapshot` has an `action` enum (create, restore, list,
  delete). This pattern keeps tool definitions compact.
- **Token analytics**: every MCP tool call is instrumented with timing and
  token estimation (`ceil(chars / 4.0)` heuristic). Analytics are stored in
  SQLite (`~/.daimonos/analytics.db`) for cross-session history with 90-day
  auto-cleanup. In-memory session stats track per-tool breakdowns, redirect
  (L1) and filter (L2) hits, and read dedup hits. Exposed via `session_stats`
  MCP tool (scopes: session, history, daily), `workspace_info` analytics
  summary, and `daimonos --stats` CLI flag. Configure or disable via
  `[analytics]` in `daimonos.toml`.
- **Compact responses**: all MCP tool responses use compact JSON
  (`to_string`, not `to_string_pretty`). Redundant fields (`count` on
  arrays, `size` on writes) are omitted. Empty `err` fields in exec
  responses are skipped.

## Testing

**Test-driven development is required.** When building new functionality, write
tests as part of the same change — not as a follow-up task. Specifically:

- **New opcodes or handlers**: add Rust unit tests in the module's
  `#[cfg(test)] mod tests` block covering success paths, error paths, and edge
  cases. If the opcode is exposed as an MCP tool, also add a pytest case in
  the appropriate `tests/test_*.py` file.
- **New MCP tools**: add both a Rust test for the underlying handler logic and
  a pytest test that exercises the tool end-to-end over JSON-RPC.
- **Bug fixes**: add a regression test that would have caught the bug before
  applying the fix.
- **Run both suites** (`cargo test` and `python3 -m pytest tests/ -v`) before
  considering a change complete. Both must pass.

A PR or change that adds functionality without corresponding tests is
incomplete.

The test suite has two layers:

### Layer 1: Rust unit/integration tests

Tests live inline as `#[cfg(test)] mod tests` in each module. They cover
protocol parsing, session state, file operations, exec lifecycle, trigram
indexing, and config loading. All filesystem tests use `tempfile` crates for
isolation.

```bash
cargo test
```

### Layer 2: pytest MCP protocol conformance

End-to-end tests that spawn the real binary, perform the MCP handshake, and
exercise all 8 tools via JSON-RPC over stdio.

```bash
# Install deps (one time)
pip install -r tests/requirements.txt

# Run
python3 -m pytest tests/ -v
```

The `daimonos` pytest fixture (`tests/conftest.py`) builds the binary once per
session, spawns a fresh subprocess per test, performs the initialize/initialized
handshake, and provides a `DaimonosClient` with `call_tool()`, `list_tools()`,
and `send_raw()` methods.

### Manual testing

```bash
# --- MCP mode (stdio, for Cursor integration) ---
# Cursor mcp.json entry:
# { "daimonos": { "command": "/path/to/daimonos", "args": ["--mcp", "-w", "/path/to/workspace"] } }

# --- Socket mode (direct opcode protocol) ---
# Start daemon
./target/release/daimonos --workspace /path/to/workspace --debug

# Send a request (from another terminal)
echo '{"c":255}' | socat - UNIX-CONNECT:/tmp/daimonos.sock

# Batch request
echo '{"batch":[{"c":4,"p":"Cargo.toml"},{"c":6,"p":"fn main","n":5}]}' | \
  socat - UNIX-CONNECT:/tmp/daimonos.sock
```

## Linear project management

Keep the Linear project accurate **as you work**, not after being asked.

- **Before starting work**: check if a relevant issue exists. If not, create one
  and set it to "In Progress."
- **While working**: if scope changes or you discover sub-tasks, update the issue
  description or create child issues.
- **After completing work**: mark the issue "Done." If the work produced
  decisions, trade-offs, or research worth preserving, add or update a Linear
  document in the daimonos project.
- **If you create new files or opcodes**: make sure the issue description
  reflects what was actually built, not just what was planned.
- **Never backfill** a batch of issues after the fact. Each piece of work should
  have an issue created before or at the start of that work.

The user should be able to open the daimonos project in Linear at any time and
see an accurate picture of what's done, what's in progress, and what's next.

## Review checklist

When reviewing or producing a diff:

- **Config**: no hardcoded strings that could change (extensions, paths,
  limits). These belong in `daimonos.default.toml`.
- **Protocol**: new opcodes must be added to all three places: `protocol::op`,
  dispatcher in `ops/mod.rs`, schema in `ops/schema.rs`.
- **Responses**: must be structured JSON. No raw text output.
- **Error handling**: no panics, no unwrap() in request paths. Use
  `Response::err()`.
- **Tests**: new functionality must include tests in the same change (Rust
  unit tests + pytest MCP tests where applicable). Both `cargo test` and
  `python3 -m pytest tests/ -v` must pass.
- **Dependencies**: flag new crate additions for confirmation.
