# AGENTS.md

Cross-tool guidance for AI coding agents working in this repository.

## Quick start (new agents)

**Do these in order. Do NOT skip to reading source code.**

1. Read this file fully — project context, repo layout, conventions.
2. **Go to Vikunja first.** Use the Vikunja MCP tools to read the daimonos
   projects — see [Vikunja projects](#vikunja-projects) for the id map. This is
   the authoritative source for what's been done, what's in progress, and what's
   planned. Most active work lives in project **183 (`daimonos-agent`)**.
   Understand the project state before touching the codebase.
3. Check `.cursor/rules/` — scoped rule files (bounded collections,
   configurable limits, env inheritance, no thread-spawn in callbacks,
   resource lifecycle, full-lifecycle tests, repowise).
4. Check `.cursor/strategies/` — Rust collection patterns, memory-safety
   checklist, testing strategies.
5. Check `docs/` for technical specs, and `docs/adr/` for the accepted
   architecture decision records.
6. **Only then** read source code as needed for your specific task.

## Daimonos tool usage policy

For repository/workspace operations in this project, use the daimonos MCP
server tools by default. This includes file reads/writes, search, exec, git,
cargo, docker, snapshots, and workspace introspection.

- Prefer `execute_script` when a task needs 2+ daimonos tool calls.
- Use single daimonos tools only for one-off operations.
- Use non-daimonos tools only when the target system is external to the
  workspace (for example Vikunja, GitHub, Atlassian, or web search).

> **Note:** The workspace is the default base for relative paths and the
> trigram search index. It is **not** a filesystem boundary — daimonos can
> read, write, and exec any path on the system. There is no confinement.

## What is Daimonos?

Daimonos is a **bare-metal operating system purpose-built for AI agents**. The
kernel, filesystem, process model, and syscall interface are designed from the
ground up for agent workloads — no shell, no terminal, no human-facing
interfaces.

The name comes from Greek *daimon* (agent/spirit), the etymological root of
"daemon."

**The current codebase is Phase 1: a user-space prototype** running on Linux.
It proves out the opcode protocol and structured I/O design before they become
native syscall interfaces in the eventual bare-metal OS.

Phase 1 has grown past a bare tool server. The same binary now ships both
halves of the stack:

- **Tool server** — the opcode protocol, MCP transports (stdio + Unix socket),
  trigram index, tool plugins, snapshots, analytics.
- **Agent harness** — a first-party agent loop (`src/agent.rs`) with LLM
  providers (Anthropic, OpenAI, OpenRouter), context compaction, safety
  approvals, session persistence, OpenTelemetry observability, and three
  frontends: one-shot `agent`, interactive `chat`, and a native ACP engine
  (`src/acp_cmd.rs`) that drives Zed and other ACP clients.

See `docs/runtime-modes.md` for the six runtime modes and how they are selected.

For the full vision (kernel, StructFS, capability security, phased roadmap),
read the project descriptions in Vikunja.

## Systems of record

| What | Where |
|------|-------|
| Vision, roadmap, project status | Vikunja projects (see below) |
| Research findings and analysis | Vikunja tasks (findings are filed as tasks) |
| Task status and ownership | Vikunja tasks |
| Install & setup guides | `docs/install.md` + per-tool guides in `docs/` |
| Protocol specification | `docs/protocol.md` |
| Runtime modes | `docs/runtime-modes.md` |
| Architecture decisions | `docs/adr/` (numbered ADRs) |
| Configuration reference | `docs/configuration.md` and `daimonos.default.toml` |
| Observability | `docs/observability.md` + ADR-006 |
| Coding rules and conventions | This file + `.cursor/rules/` |

Technical documentation that doesn't fit in Vikunja (specs, diagrams,
benchmarks, design docs) goes in `docs/`. Use Vikunja for research findings,
status reports, and narrative context. Use `docs/` for anything an agent or
developer needs while reading or writing code. Architecture decisions get a
numbered ADR in `docs/adr/`, referenced from the Vikunja task.

### Vikunja projects

A search for "daimonos" matches many projects. The ones that matter:

| Id | Project | Scope |
|----|---------|-------|
| **183** | `daimonos-agent` | **Primary.** Agent harness, ACP engine, providers, agent mail. Most active work. |
| 30 | `daimonos` | Core tool server: shared daemon, trigram indexing, model-training research. |
| 172 | Agentic CLI tool | `daimonos agent` / `chat` CLI ergonomics. |
| 12 | Daimonos | Umbrella / cross-references / distribution listings. |
| 25 | Daimonos Documentation | Docs tasks. |
| 29 | daimonos Linux Distro | Buildroot image, kernel config, AWS/QEMU. |
| 28 | daimonos Bare-Metal Agent OS | The eventual OS phase. |
| 78 | Daimonos growth and distribution | Launch, directories, outreach. |
| 91 / 92 / 93 | KGL | Knowledge Graph Language: design, market research, v0 build (`src/kgl/`). |
| 86 | Graphify Evaluation & Integration | `graphify-out/` knowledge graph. |
| 110 / 178 / 181 | Research | Competitive eval, benchmark harness, verbosity/token levers. |

Code and commits reference Vikunja task ids as `#<id>` (for example
`fix(#1070): ...`, and module headers such as `//! ... (vikunja #954)`). These
are **Vikunja ids, not GitHub issue numbers** — this repo tracks no GitHub
issues. GitHub PR numbers appear separately, e.g. `(#94)`.

> **Caveat when querying Vikunja:** the MCP tool's `filter: "done = false"` is
> applied client-side to one page at a time, so it silently reports zero open
> tasks for projects whose open items fall on a later page. Page through with
> `page`/`perPage` and filter the results yourself instead of trusting that
> filter.

## Repo layout

```
daimonos/
├── AGENTS.md                      # This file (agent guidance)
├── README.md                      # Project overview for humans
├── INSTALL.md                     # Install paths and packaging
├── CONTRIBUTING.md, SECURITY.md, CHANGELOG.md, CODEOWNERS
├── Cargo.toml                     # Rust manifest (edition 2021)
├── rust-toolchain.toml            # Pinned toolchain
├── daimonos.default.toml          # Reference config with all tunable values
├── Dockerfile                     # Container image (also used by Glama)
├── server.json                    # GitHub MCP Registry metadata
├── glama.json                     # Glama listing metadata
├── osv-scanner.toml               # Vulnerability scanner config
├── .cursor/
│   ├── rules/                     # Scoped rule files (bounded collections,
│   │                              #   configurable limits, env inheritance,
│   │                              #   no thread-spawn in callbacks, resource
│   │                              #   lifecycle, full-lifecycle tests, repowise)
│   └── strategies/                # Rust collection patterns, memory-safety
│                                  #   checklist, testing strategies
├── .git-hooks/                    # Repo hooks (_index-sync refreshes
│                                  #   repowise + the graphify KGL substrate)
├── .github/
│   ├── RELEASE_TEMPLATE.md
│   └── workflows/                 # ci.yml, deploy.yml, distro.yml, release.yml
├── graphify-out/                  # Local KGL substrate, gitignored (ADR 012)
├── .repowise/                     # Local repowise index, gitignored
├── mcpb/manifest.json             # MCPB bundle manifest template
├── prompts/                       # Model-facing text, embedded via include_str!
│   ├── README.md                  # How overrides work
│   ├── agent_system.md            # Agent-loop system prompt
│   ├── mcp_instructions.md        # MCP `instructions` field
│   ├── summary.md                 # Compaction summary prompt
│   ├── kgl_hint.md                # KGL orientation nudge
│   └── tool_descriptions.toml     # Top-level tool descriptions
├── scripts/cursor-review.sh
├── benchmarks/                    # Token/runtime benchmark harness
│   ├── bench-agent.sh, bench-cursor.sh   # per-runtime runners
│   ├── analyze.py, summarize.py          # cross-run analysis + per-run table
│   ├── extract_tokens.py, check_task.py  # token normalizer + correctness gate
│   ├── server-bench/              # Server-side benchmark
│   ├── tasks/                     # Task definitions (JSON)
│   └── workspace/                 # Target codebase (Rust inventory app)
├── distro/
│   ├── build-buildroot.sh         # Build script: Buildroot disk image (canonical)
│   ├── test-qemu.sh               # Boot image in QEMU for local testing
│   ├── smoke-test.sh              # Image smoke test
│   ├── deploy-aws.sh              # Deploy image to AWS (S3 + AMI)
│   ├── br2-external/              # Buildroot external tree (defconfig, overlay, board)
│   └── alpine-legacy/             # Deprecated Alpine build (reference only)
├── docs/
│   ├── adr/                       # Numbered architecture decision records
│   │   ├── 001-provider-boundary-invariant.md
│   │   ├── 002-context-window-compaction.md
│   │   ├── 003-acp-mcp-server-bridge.md
│   │   ├── 004-acp-plan-updates.md
│   │   ├── 005-zed-session-editing-extension.md
│   │   ├── 006-llm-observability.md
│   │   ├── 007-context-offload-handles.md
│   │   ├── 008-programmatic-llm-subcalls.md
│   │   └── 009-agent-coordination.md
│   ├── protocol.md                # Opcode protocol specification
│   ├── runtime-modes.md           # The six runtime modes and how they resolve
│   ├── configuration.md           # Config file reference (all tunables)
│   ├── observability.md           # OpenTelemetry setup and span model
│   ├── install.md                 # Build & install instructions
│   ├── aws-nitro-kernel-config.md # Distro kernel requirements on AWS Nitro
│   └── *-setup.md                 # Per-client integration guides: zed, zed-acp,
│                                  #   cursor, claude-code, copilot, cline,
│                                  #   windsurf, gemini-cli, discord, herdr,
│                                  #   other-tools
├── tests/                         # pytest MCP conformance suite (~40 modules)
│   ├── conftest.py                # Fixture: builds binary, MCP handshake, DaimonosClient
│   ├── requirements.txt
│   ├── test_handshake.py, test_read_write.py, test_edit.py, test_search.py,
│   ├── test_exec.py, test_ls.py, test_errors.py, test_symlinks.py, test_diff.py
│   ├── test_batch.py, test_snapshots.py, test_set_cwd.py, test_roots.py
│   ├── test_git.py, test_plugins.py, test_npm.py, test_curl.py, test_shellcheck.py
│   ├── test_tool_pipeline.py, test_tool_list_changed.py, test_workspace_info.py
│   ├── test_mcp_socket.py, test_cli_modes.py, test_cli_prompts.py
│   ├── test_acp_lifecycle.py, test_lifecycle.py, test_memory.py
│   ├── test_analytics.py, test_observability.py, test_logging.py
│   ├── test_index_guard.py, test_kgl.py, test_kgl_observe.py
│   └── test_bench_*.py, test_benchmark_scripts.py, test_server_bench_smoke.py
└── src/
    ├── main.rs                    # Entrypoint: dispatches the resolved runtime mode
    ├── cli.rs                     # clap defs + RuntimeMode (subcommands & legacy flags)
    │
    │   # ---------- Agent harness ----------
    ├── agent.rs                   # Core agent loop: run(), AgentSession, tool dispatch,
    │                              #   plan entries, usage accounting, retries
    ├── agent_runtime.rs           # Mode entrypoints (run_agent/run_chat/run_acp) +
    │                              #   provider construction from the agent env
    ├── agent_cmd.rs               # One-shot `agent` frontend
    ├── chat_cmd.rs                # Interactive `chat` REPL (reedline)
    ├── acp_cmd.rs                 # Native ACP engine over stdio (Zed & other ACP clients)
    ├── agent_env.rs               # Agent env file: provider, model, keys, approvals, compaction
    ├── herdr.rs                   # Herdr pane supervision: semantic state reports from chat/agent
    ├── compaction.rs              # Context-window compaction (ADR-002)
    ├── safety.rs                  # Approval modes, allow/deny gates, persisted approvals
    ├── session_store.rs           # On-disk conversation persistence (chat + ACP sessions)
    ├── mcp_bridge.rs              # Outbound MCP client: external MCP servers as agent tools (ADR-003)
    ├── zed_config.rs              # Reads Zed `context_servers` settings as an MCP fallback
    ├── providers/
    │   ├── mod.rs                 # LlmProvider trait, Message/ContentBlock/Usage/StopReason
    │   ├── anthropic.rs           # Anthropic Messages API
    │   ├── openai.rs              # Native OpenAI Responses API
    │   └── openrouter.rs          # OpenRouter chat/completions
    │
    │   # ---------- Tool server ----------
    ├── mcp.rs                     # MCP server handler (stdio + Unix socket transports)
    ├── protocol.rs                # Request/Response types, opcode constants
    ├── session.rs                 # Per-connection session state (cwd, env, caches, exposure)
    ├── tools.rs                   # ToolDef registry: single source of truth for tool definitions
    ├── tool_facade.rs             # Provider-neutral tool schemas for the agent loop
    ├── tool_descriptions.rs       # Runtime-configurable tool/parameter descriptions
    ├── tool_runner.rs             # ToolPlugin trait, registry, repair loop
    ├── script.rs                  # Starlark interpreter: execute_script + tool bindings
    ├── config.rs                  # TOML config loading, tool registration
    ├── prompts.rs                 # Externalized model-facing prompts + overrides
    ├── paths.rs                   # Shared process-level path resolution
    ├── verbosity.rs               # Per-session output verbosity level
    ├── index.rs                   # Background trigram workspace indexer
    ├── pipeline_cache.rs          # inotify-based tool output cache
    ├── snapshot.rs                # Workspace snapshot store
    ├── analytics.rs               # Token analytics: SQLite per-tool-call tracking
    ├── observability.rs           # OpenTelemetry tracer/spans/exporters (ADR-006)
    ├── logging.rs                 # tracing subscriber; 0700 log dir, 0600 files
    ├── ops/
    │   ├── mod.rs                 # Opcode dispatcher; also handles find(7),
    │   │                          #   env_set(16), env_get(17), session(18)
    │   ├── file_ops.rs            # 0-6: read, write, patch, ls, stat, glob, grep
    │   ├── exec_ops.rs            # 8-11: exec, bg, poll, kill
    │   ├── exec_filter.rs         # Semantic exec output filters (test, build, install, lint)
    │   ├── snap_ops.rs            # 12-13, 25-26: snap, restore, snap_list, snap_delete
    │   ├── diff_ops.rs            # 14: diff (in-process structured diffing)
    │   ├── coord.rs               # 19: agent coordination (ADR-009)
    │   ├── tool_ops.rs            # 20-24: tool_run, repair, pipeline, register, list
    │   └── schema.rs              # 255: self-describing schema registry
    ├── coordination/              # Agent-to-agent "agent mail" (ADR-009): per-workspace WAL SQLite
    │   ├── mod.rs                 # Workspace-keyed DB path (~/.daimonos/coordination/<hash>.db)
    │   ├── store.rs               # CoordinationStore: identity, messages/inbox/threads, reservations
    │   └── names.rs               # Memorable AdjectiveNoun agent-name minting
    ├── kgl/                       # Knowledge Graph Language v0 (code + OS graph layer)
    │   ├── mod.rs, model.rs       # Module root; core data model
    │   ├── store.rs               # SQLite side-store for the graph
    │   ├── query.rs, assert.rs    # kgl_query (read) / kgl_assert (write) surfaces
    │   ├── observe.rs             # Observed-provenance capture
    │   ├── autoindex.rs, demo.rs  # Startup auto-indexing; v0 orient demo
    │   └── substrate*.rs          # Substrate abstraction + graphify and x07 backends
    └── plugins/
        ├── mod.rs                 # Plugin module
        ├── generic_cli.rs         # Generic plugin for any CLI tool with JSON output
        ├── git.rs, cargo.rs       # Auto-registered when git / Cargo.toml detected
        ├── gh.rs, docker.rs       # GitHub CLI; Docker
        ├── npm.rs, pytest.rs      # npm; pytest
        ├── shellcheck.rs, curl.rs # shellcheck; structured HTTP
        ├── discord.rs             # Discord
        └── x07.rs                 # X07-specific plugin (semantic indexing, decl cache)
```

## Prerequisites

| Dependency | Required by | Install |
|-----------|-------------|---------|
| Rust (stable) | Building daimonos | `rustup` |
| `socat` | Manual testing via Unix socket | system package manager |
| Python 3 + pytest | MCP protocol tests | `pip install -r tests/requirements.txt` |
| Agent env file + provider API key | `agent`, `chat`, `acp` modes | `~/.config/daimonos/agent.env` (or `$DAIMONOS_AGENT_ENV`) |

The tool-server modes (`mcp`, `daemon`) need no API key. The agent modes load
provider, model, key, approval mode, and compaction settings from the agent env
file — see `src/agent_env.rs` for the recognized keys. A CLI `--provider` /
`--model` flag overrides the file.

## Coding conventions

- **Language**: Rust, edition 2021
- **Async runtime**: Tokio
- **Serialization**: serde + serde_json for protocol, toml for config
- **No hardcoded values**: all tunables go in `daimonos.default.toml` and
  `config.rs`. Never hardcode file extensions, limits, or defaults in
  operational code.
- **No hardcoded model-facing text**: prompts live in `prompts/*.md` and
  top-level tool descriptions in `prompts/tool_descriptions.toml`. Both are
  embedded via `include_str!` and overridable through `[prompts]` (see
  `prompts/README.md`). Never inline a new prompt or top-level tool description
  as a Rust string literal; update the corresponding resource and override
  support instead.
- **Compact field names**: protocol fields use single-letter keys (`c`, `p`,
  `s`, `n`, `a`, `g`, `kv`) to minimize token cost. Response fields are
  similarly terse (`d` for data, `e` for error code, `m` for message).
- **Structured output only**: every response is JSON. Never return unstructured
  text. If a subprocess produces text, wrap it in `{"raw": "..."}`.
- **Error handling**: use `Response::err(code, msg)` with numeric error codes
  (1=not found, 2=permission, 3=invalid arg, 4=IO, 5=process, 6=timeout,
  7=snapshot not found). No panics in request handlers.
- **New opcodes**: assign the next available number, add to `protocol::op`,
  add handler in `ops/mod.rs`, add schema entry in `ops/schema.rs`. Note
  `op::GIT` (15) is a **reserved, undispatched leftover** from before git
  became a plugin — do not build on it.
- **Provider boundary (ADR-001)**: provider modules own wire-format
  translation only. Loop control, tool dispatch, compaction, and safety
  decisions live above the `LlmProvider` trait, never inside a provider.
  Adding a provider means implementing the trait plus registering it in
  `agent_runtime::try_build_provider`; it must not require agent-loop changes.
- **Architecture decisions**: a change to the agent harness's structure gets a
  numbered ADR in `docs/adr/`, referenced from its Vikunja task.
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
- **Tool tiers** (`ToolTier` in `tools.rs`) — four levels, and they decide
  what `list_tools` shows:
  - `Full` — always present with a complete JSON Schema.
  - `Terse` — name + description only; full schema on demand via
    `get_tool_schema`. Most plugin tools (`git`, `cargo`, `docker`, `gh`,
    `snapshot`, ...) live here to save prefix tokens. Setting
    `DAIMONOS_MCP_FULL_SCHEMAS=1` or `[mcp] full_tool_schemas = true`
    promotes them to full schemas (needed by introspecting directories such
    as Glama).
  - `OnDemand` — hidden until activated: `list_tool_signatures`,
    `diff_files`, `tool_pipeline`, `lint_repair`.
  - `AgentOnly` — see the next bullet.
- **Lazy tool exposure**: the initial `list_tools` response contains every
  `Full` and `Terse` tool (`tools::initial_exposed_tools()`); only
  `OnDemand` tools are withheld, and they appear once the model calls
  `list_all_tools` or uses one directly. Tools with a `context_check` are
  additionally hidden when their prerequisite is missing — `git`/`gh` need
  `.git`, `cargo` needs `Cargo.toml`, `kgl_query`/`kgl_assert` need `.kgl`,
  and `pytest`/`docker` have their own probes. The exposed set is tracked in
  `session.exposed_tools`. When a `tools/call` grows that set, the server
  emits a `notifications/tools/list_changed` (both stdio and socket
  transports advertise `tools.list_changed: true`), so clients re-fetch
  `tools/list`. A `Session::tools_changed` dirty flag drives this and is
  set only on a real membership addition — not on the description re-render
  that happens on a tool's first use.
- **Agent-only tools**: `ToolTier::AgentOnly` entries are available to the
  built-in agent/chat/ACP loop but excluded from MCP `tools/list`,
  `list_all_tools`, and MCP schema lookup. Use this only for frontend-local
  side effects with no portable MCP meaning (currently `update_plan`).
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
- **Agent-harness changes**: the agent loop, ACP engine, providers, compaction,
  and safety are covered by inline Rust tests — `acp_cmd.rs` and `agent.rs`
  alone carry well over a hundred. Add tests beside the existing ones in the
  module you touch; add a `tests/test_acp_lifecycle.py` or `test_cli_modes.py`
  case when the behavior is observable from outside the process.
- **Bug fixes**: add a regression test that would have caught the bug before
  applying the fix.
- **Run both suites** (`cargo test` and `python3 -m pytest tests/ -v`) before
  considering a change complete. Both must pass. `cargo clippy -- -D warnings`
  and `cargo fmt` must also be clean.

A PR or change that adds functionality without corresponding tests is
incomplete.

The test suite has two layers:

### Layer 1: Rust unit/integration tests

Tests live inline as `#[cfg(test)] mod tests` in each module. They cover
protocol parsing, session state, file operations, exec lifecycle, trigram
indexing, and config loading, plus the agent harness: the agent loop, ACP
engine, providers, compaction, safety gates, and the MCP bridge. All filesystem
tests use `tempfile` crates for isolation.

```bash
cargo test
```

### Layer 2: pytest MCP protocol conformance

End-to-end tests that spawn the real binary, perform the MCP handshake, and
exercise the tool surface via JSON-RPC over stdio. Roughly 40 modules now
cover the tools, both MCP transports, CLI mode resolution, ACP and process
lifecycle, logging, analytics, observability, KGL, and the benchmark scripts.

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
# --- MCP mode (stdio, for editor integration) ---
# mcp.json entry:
# { "daimonos": { "command": "/path/to/daimonos", "args": ["mcp", "-w", "/path/to/workspace"] } }

# --- Daemon mode (direct opcode protocol) ---
./target/release/daimonos --workspace /path/to/workspace --debug daemon

# Send a request (from another terminal)
echo '{"c":255}' | socat - UNIX-CONNECT:/tmp/daimonos.sock

# Batch request
echo '{"batch":[{"c":4,"p":"Cargo.toml"},{"c":6,"p":"fn main","n":5}]}' | \
  socat - UNIX-CONNECT:/tmp/daimonos.sock

# --- Agent modes ---
./target/release/daimonos agent "<task>" --dry-run   # no API call; prints tools + task
./target/release/daimonos chat                       # interactive REPL
./target/release/daimonos chat --list                 # saved chat sessions
./target/release/daimonos acp                         # ACP engine over stdio (Zed)
```

After changing the ACP engine, a live check means installing the binary and
restarting the ACP client (Zed) — the client spawns `daimonos acp` itself, so a
stale binary silently keeps serving the old behavior.

## Vikunja project management

Keep Vikunja accurate **as you work**, not after being asked. Work goes in
project **183 (`daimonos-agent`)** unless it clearly belongs to one of the other
projects in the id map above.

- **Before starting work**: check whether a relevant task exists. If not, create
  one. Remember the `done = false` filter caveat — page through instead.
- **While working**: if scope changes or you discover sub-tasks, update the task
  description or create related tasks.
- **After completing work**: mark the task done and record the commit. If the
  work produced decisions or trade-offs worth preserving, write a numbered ADR
  in `docs/adr/` and reference it from the task.
- **Reference the task id in the commit subject**, e.g.
  `fix(#1070): persist ACP turn timestamps in session history`.
- **If you create new files or opcodes**: make sure the task description
  reflects what was actually built, not just what was planned.
- **Never backfill** a batch of tasks after the fact. Each piece of work should
  have a task created before or at the start of that work.
- **Close tasks when the work merges.** Shipped-but-still-open tasks are the
  most common drift in this project; check for near-duplicates before filing.

The user should be able to open the daimonos projects in Vikunja at any time and
see an accurate picture of what's done, what's in progress, and what's next.

## Review checklist

When reviewing or producing a diff:

- **Config**: no hardcoded strings that could change (extensions, paths,
  limits). These belong in `daimonos.default.toml`.
- **Protocol**: new opcodes must be added to all three places: `protocol::op`,
  dispatcher in `ops/mod.rs`, schema in `ops/schema.rs`.
- **Provider boundary**: no loop control, tool dispatch, or compaction logic
  inside `providers/*` (ADR-001).
- **Prompts**: no new model-facing string literals in Rust — they belong in
  `prompts/`.
- **Vikunja**: the task exists, references the commit, and is closed when the
  work lands.
- **Responses**: must be structured JSON. No raw text output.
- **Error handling**: no panics, no unwrap() in request paths. Use
  `Response::err()`.
- **Tests**: new functionality must include tests in the same change (Rust
  unit tests + pytest MCP tests where applicable). Both `cargo test` and
  `python3 -m pytest tests/ -v` must pass.
- **Dependencies**: flag new crate additions for confirmation.
