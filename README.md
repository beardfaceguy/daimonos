# Daimonos

[![CI](https://github.com/beardfaceguy/daimonos/actions/workflows/ci.yml/badge.svg)](https://github.com/beardfaceguy/daimonos/actions/workflows/ci.yml)
[![Latest Release](https://img.shields.io/github/v/release/beardfaceguy/daimonos?display_name=tag)](https://github.com/beardfaceguy/daimonos/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![MCP Registry](https://img.shields.io/badge/MCP-Registry-blue)](https://github.com/modelcontextprotocol/registry)

**An agent-optimized OS layer that makes AI coding agents faster and cheaper.**

Daimonos replaces the built-in file, search, exec, and git tools in your AI
coding agent with structured equivalents that return compact JSON instead of
raw terminal output. The result: fewer tokens consumed, fewer round-trips, and
lower API costs — with zero changes to your workflow.

**Platforms:** Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel).
Windows support is [planned](https://linear.app/clawcorp/issue/CLA-302).

For repository agent/operator conventions, see `AGENTS.md` (especially
**Daimonos tool usage policy**).

The name comes from Greek *daimon* (agent/spirit), the etymological root of
"daemon."

## The problem

When an AI agent runs `cargo test`, it gets back hundreds of lines of terminal
output — progress bars, compile messages, passing test names — when all it
needs is "47 passed, 0 failed." The agent pays for every token of that noise:
reading it, reasoning about it, and carrying it in context for the rest of the
session.

The same waste happens with `git status`, `docker ps`, `ls -la`, and every
other shell command. Agents spend 30-50% of their token budget on verbose,
unstructured tool output.

## How it works

Daimonos runs as an [MCP server](https://modelcontextprotocol.io/) that your
IDE or CLI spawns automatically. It provides the same operations agents already
use — read files, write files, search, execute commands, git operations — but
returns compact, structured JSON instead of raw text.

The single binary also provides ACP, one-shot agent, interactive chat, and
socket-daemon runtimes — a full coding-agent harness in its own right; see
[Agent harness features](#agent-harness-features) below and
[Runtime modes](docs/runtime-modes.md) for the explicit subcommands and
compatibility aliases.

```
Agent: exec("cargo test")

Without Daimonos (raw terminal output):
   Compiling inventory v0.1.0 (/workspace)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.31s
     Running unittests src/main.rs (target/debug/deps/inventory-abc123)
running 47 tests
test config::tests::test_default ... ok
test config::tests::test_load ... ok
... (200+ more lines)
test result: ok. 47 passed; 0 failed; 0 ignored

With Daimonos (structured JSON):
{"ok":true,"tests":47,"passed":47,"failed":0,"failures":[]}
```

### Four layers of optimization

1. **Native tool plugins** — `git`, `cargo`, `gh`, and `docker` are exposed as
   first-class MCP tools with structured JSON output. When agents call
   `exec("cargo test")`, Daimonos intercepts it and routes through the native
   plugin instead.

2. **Semantic output filters** — For commands without native plugins (pytest,
   make, pip install, eslint, etc.), Daimonos applies semantic compression:
   test runners return summary + failures only, build commands return "ok" or
   just the errors, install commands return success/failure.

3. **Protocol-level efficiency** — Read deduplication (re-reading an unchanged
   file returns `{"unchanged":true}` instead of the full content), compact
   field names, lazy tool exposure, batch operations, and a terse output
   directive that cuts LLM prose by ~30%.

4. **Managed subprocess execution** — Command output is bounded while it is
   read instead of after full buffering. Daimonos owns Unix process groups,
   retires descendants on cancellation or session shutdown, isolates child
   environments through an explicit allowlist, and stores background output
   in private bounded artifacts.

## Benchmark results

Tested with Claude Opus 4.6 on identical coding tasks (read files, search code,
edit, run tests, git operations):

| Metric | Baseline | Daimonos | Savings |
|--------|----------|----------|---------|
| Output tokens | 5,842 | 3,198 | **-45.3%** |
| Total tokens | 41,239 | 33,847 | **-17.9%** |
| Tool calls | 17 avg | 14 avg | **-17.6%** |
| Wall time | 42.1s avg | 35.2s avg | **-16.4%** |

Remote benchmarks on AWS (same hardware, same model, same tasks) showed
**20.3% cost reduction** and **14.0% faster** task completion.

### SWE-bench Verified (mini) — three-way harness comparison

Five instances from [swe-bench-verified-mini](https://huggingface.co/datasets/MariusHobbhahn/swe-bench-verified-mini),
same model (Claude Opus 4.8) across all three harnesses, each agent running
inside the official SWE-bench Docker image for its instance (real test
environment), scored with the official `swebench` evaluation harness:

| Instance | daimonos tokens (LLM calls) | mini-swe-agent tokens (LLM calls) | cursor-agent tokens |
|---|---:|---:|---:|
| django__django-11815 | 54,755 (4) | 38,305 (9) | 113,242 |
| django__django-12155 | 53,773 (4) | 33,095 (9) | 348,425 |
| django__django-12708 | 88,765 (6) | 196,492 (22) | 451,349 |
| sphinx-doc__sphinx-8035 | 172,729 (10) | 342,108 (30) | 930,495 |
| sphinx-doc__sphinx-9367 | 66,812 (5) | 18,611 (6) | 230,223 |
| **Total tokens** | **436,834** | **628,611** | **2,073,734** |
| **Total wall time** | **84 s** | **247 s** | **279 s** |
| **Resolved** | **5/5** | **5/5** | **5/5** |

Conclusions:

- **Correctness parity**: all three harnesses resolved 5/5 at this sample
  size, so daimonos's token savings did not cost any resolutions.
- **daimonos was cheapest and fastest**: ~30% fewer tokens than
  mini-swe-agent (the minimal open-source baseline) and ~4.8x fewer than
  cursor-agent, with ~3x less wall time than either.
- **Caveats**: n=5, and run-to-run variance is real (daimonos spent 503k
  tokens on sphinx-8035 in an earlier identical-config run vs 173k here).
  cursor-agent's total is mostly cache-read tokens billed at a fraction of
  input price, so its raw token count overstates its relative cost.
  Measured API spend for the batch (OpenRouter): daimonos $1.53,
  mini-swe-agent $1.66; cursor-agent bills via Cursor's backend.

See [benchmarks/swebench/](benchmarks/swebench/) for the runners and
methodology.

## 60-second demo

Use this script for README readers, release notes, and social posts:

```bash
# 1) Install daimonos
cargo build --release
sudo cp target/release/daimonos /usr/local/bin/

# 2) Configure your MCP client (example: Cursor)
# .cursor/mcp.json -> command: daimonos, args: ["--mcp", "-w", "/path/to/project"]

# 3) Ask your agent to run:
# "Run cargo test and summarize failures only."
# "Show git status as structured output."
```

What to highlight in the demo:
- same workflows, less tool-output noise
- structured responses instead of raw terminal spam
- fewer tokens and fewer round-trips for common coding tasks

## Quick start

### Install

**Pre-built binaries** (Linux and macOS):

```bash
# Linux x86_64
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-x86_64-linux.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/

# macOS Apple Silicon
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-aarch64-macos.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/
```

**From source:**

```bash
git clone https://github.com/beardfaceguy/daimonos.git
cd daimonos
cargo build --release
sudo cp target/release/daimonos /usr/local/bin/
```

See [docs/install.md](docs/install.md) for all platforms (ARM Linux, Intel Mac,
musl static builds).

### Configure your IDE

For most users, start with one of these:

- **Cursor**: [Cursor IDE setup](docs/cursor-setup.md)
- **Zed**: [Zed setup](docs/zed-setup.md)
- **Claude Code**: [Claude Code setup](docs/claude-code-setup.md)

Add Daimonos as an MCP server. For **Cursor**, add to your project's
`.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "daimonos",
      "args": ["--mcp", "-w", "/path/to/your/project"]
    }
  }
}
```

That's it. Daimonos starts when your IDE opens the project and exits when you
close it. No daemon to manage, no background service.

### Setup guides for other tools

- [Cursor IDE](docs/cursor-setup.md)
- [GitHub Copilot](docs/copilot-setup.md) (VS Code, Visual Studio, JetBrains, Xcode, Eclipse)
- [Claude Code](docs/claude-code-setup.md) (CLI + macOS Desktop app)
- [Windsurf](docs/windsurf-setup.md)
- [Cline](docs/cline-setup.md) (VS Code extension)
- [Gemini CLI](docs/gemini-cli-setup.md)
- [Zed Editor](docs/zed-setup.md)
- [Discord integration](docs/discord-setup.md) (bot token, allowlists, read-only tools)
- [Other tools](docs/other-tools-setup.md) (Claude Desktop, ChatGPT, Continue.dev, BoltAI, etc.)

## What's included

### Core tools (always available)

| Tool | What it does |
|------|-------------|
| `read_file` | Read with optional offset/limit, content-hash deduplication |
| `write_file` | Write with auto-mkdir |
| `edit_file` | String replacement with diff confirmation |
| `search` | Regex search (content mode) or file discovery (file mode) |
| `exec` | Run commands with semantic filtering, bounded capture, and owned teardown |
| `batch` | Multiple operations in a single round-trip |
| `workspace_info` | Project type, git status, directory listing, analytics |

### Native tool plugins (auto-detected)

These appear automatically when the corresponding CLI tool is found on PATH:

| Plugin | Commands | Detected by |
|--------|----------|-------------|
| `git` | status, log, diff, branch, add, commit, push, pull, checkout | `.git` directory |
| `cargo` | test, build, check, clippy, fmt, add | `Cargo.toml` |
| `gh` | pr_view, pr_list, pr_create, pr_diff, pr_checks, api | `gh` on PATH |
| `docker` | ps, logs, exec, images, inspect, stop, compose_up/down/ps | `docker` on PATH |

### Additional capabilities

- **Workspace snapshots** — Checkpoint before risky edits, rollback on failure
- **Starlark scripting** — Bundle multiple tool calls into a single script
- **Token analytics** — Per-tool-call tracking with cross-session history (`daimonos --stats`)
- **Background processes** — Start, poll, and stop long-running commands with
  admission limits, private bounded logs, and descendant cleanup
- **Configurable** — All tunables in a single [TOML config file](docs/configuration.md)

### Managed process lifecycle

Raw `exec`, background jobs, and CLI plugins (`cargo`, `git`, `gh`, `docker`,
`npm`, `pytest`, `curl`, and `shellcheck`) share one managed execution layer:

- **Streaming-time bounds** — stdout and stderr retain UTF-8-safe head/tail
  previews without first allocating the complete output
- **Process-group ownership on Unix** — cancellation and shutdown send TERM,
  wait a configurable grace period, then escalate to KILL and reap descendants
- **Secure background artifacts** — random exclusive `0600` files under a
  private `0700` directory, with configurable byte and job-count limits
- **Environment isolation** — children inherit only configured parent
  variables plus explicit session, tool, and per-call overrides; provider and
  MCP credentials are not ambiently leaked
- **Structured-output integrity** — plugins reject truncated JSON rather than
  reporting an incomplete result as valid

## Agent harness features

Beyond the MCP server, the same binary is a complete coding-agent harness:
an interactive terminal UI (`daimonos agent`), an ACP backend for Zed, a
one-shot CLI, and a session daemon with attach/detach and remote control.

Many of its recent features come from a systematic study of 60+ open-source
agent harnesses (Aider, OpenHands, SWE-agent, Goose, OpenCode, Forge, Pi,
the Cline family, and others) — mining the ecosystem for proven techniques
and adapting the best ones.

### Provider resilience — a hiccup never kills the turn

- **Bounded provider retries** with backoff for transient failures (429/5xx/
  network), classified at the provider boundary — fatal auth/validation
  errors surface immediately
- **Automatic model failover** — on a sustained overload the turn continues
  on the next model in the chain, then returns to your preferred model on
  the next turn
- **Turn-level error resume** — when retries and failover are spent, the
  agent pauses, repairs the conversation (keeping partial streamed output),
  and continues where it left off; recovery actions surface in the UI
- **Retry-storm detection** — fingerprints repeated identical tool calls and
  steers the model out of loops
- **Orphan tool-call repair** — max-token truncation mid-tool-call is
  repaired instead of poisoning the session

### Multi-provider sessions

- **Several providers, one session** — configure Anthropic, OpenAI, and
  OpenRouter side by side (`DAIMONOS_AGENT_<NAME>_API_KEY`); every call is
  routed to the right provider by model, with an explicit `provider:slug`
  override
- **Live model discovery** — at startup the configured provider(s) are
  queried for their full model catalogs; the model picker and failover
  chain always reflect what is actually served, newest first
- **Cross-provider failover** — with more than one provider configured, an
  outage at one can fail over to models at another, mid-turn
- **Provider-reported context windows** — compaction thresholds derive from
  the live model metadata instead of hardcoded numbers

### Context economy at the harness level

- **Conversation compaction** — summarize-and-continue with high/low
  water-mark thresholds and provider-honest token accounting
- **Bounded tool results** — oversized tool output is capped at the dispatch
  boundary and offloaded to files the agent can re-read selectively
- **Reverse-budget pruning** — old tool results shrink before new ones, so a
  long turn keeps its recent working set sharp
- **Distilled working memory** — durable facts/snippets/notes that survive
  compaction, separate from the transcript
- **Resilient edit matching** (mined from Aider) — whitespace-tolerant
  search/replace cuts failed-edit retry costs
- **Batched scripting** — the agent is steered to bundle multi-step tool
  work into single Starlark scripts (~2.2x cost lever, benchmarked)

### Session durability and control

- **Per-turn workspace checkpoints** — automatic snapshots with diff/compare
  and code-only restore
- **Daemon-owned sessions** — detach from a running agent, reattach later
  (or from another terminal), with a reconnect event ring and canonical
  snapshot recovery
- **Persistent terminal UI** — streaming output, tool-lifecycle cards,
  approval modal, model/usage status bar, and vim-style scrollback
- **Remote control** — paired Android controller over an authenticated WSS
  gateway
- **Subagent delegation** — drive external ACP agents (cursor-agent,
  codex-acp, …) as delegated workers
- **Thought capture** — opt-in local persistence of streamed model reasoning
  for later inspection

Agent-mode configuration lives in a dotenv-style `agent.env`
(`~/.config/daimonos/agent.env`); see [Runtime modes](docs/runtime-modes.md).

## Architecture

Daimonos is a single Rust binary with two planes that share one tool
implementation, one opcode protocol, one config, and one analytics store:

1. **Tool server** — speaks [MCP](https://modelcontextprotocol.io/) over
   stdio (or a Unix socket) to an external agent. Your IDE spawns it as a
   subprocess — no network, no containers, no setup beyond a JSON config
   entry.
2. **Agent harness** — runs the agent loop itself, dispatching those same
   tools in-process (no MCP hop) and talking to LLM providers directly.

### Tool-server plane

```
┌──────────────┐     MCP (JSON-RPC over stdio)     ┌─────────────────┐
│  AI Agent    │ ◄──────────────────────────────► │   Daimonos      │
│  (Cursor,    │                                    │                 │
│   Copilot,   │     Structured JSON responses      │  ┌───────────┐ │
│   Claude,    │ ◄──────────────────────────────── │  │ File ops  │ │
│   etc.)      │                                    │  │ Search    │ │
│              │                                    │  │ Exec      │ │
│              │                                    │  │ Git       │ │
│              │                                    │  │ Cargo     │ │
│              │                                    │  │ Docker    │ │
│              │                                    │  │ GitHub    │ │
│              │                                    │  │ Snapshots │ │
│              │                                    │  │ Analytics │ │
└──────────────┘                                    │  └───────────┘ │
                                                    └─────────────────┘
```

Under the hood, Daimonos uses an opcode-based protocol where each operation
has a numeric identifier and compact field names (`c`, `p`, `s`, `n`) to
minimize token overhead. The MCP layer translates between standard JSON-RPC
and the internal opcode format.

### Agent-harness plane

```
┌────────────────────────────────────────────────────────┐
│  Frontends: TUI · ACP (Zed) · one-shot CLI · chat REPL │
│             session daemon (attach/detach, Android)    │
├────────────────────────────────────────────────────────┤
│  Shared session core: agent loop · canonical events    │
│  compaction · tool-result bounding · working memory    │
│  checkpoints · approvals/safety policy                 │
├────────────────────────────────────────────────────────┤
│  Provider layer: retries · model failover · resume     │
│  multi-provider router (routes each call by model)     │
│     ├─ Anthropic adapter                               │
│     ├─ OpenAI adapter                                  │
│     └─ OpenRouter adapter                              │
└────────────────────────────────────────────────────────┘
```

Every frontend drives the same transport-independent session core, so a
conversation started in the TUI can detach to the daemon and be reattached
from another terminal or a paired phone. Provider adapters own all
provider-specific wire format and error classification; everything above
them sees one `LlmProvider` interface and plain model strings — which is
what makes failover, live model discovery, and multi-provider routing
composable rather than special-cased.

## Project vision

Daimonos is being built in three phases:

### Phase 1: User-space MCP server + agent harness (current)

A Rust binary that runs on any Linux or macOS machine, in two roles that
prove out the same protocol design and structured I/O patterns:

- **Tool server** for third-party agents (Cursor, Copilot, Claude Code,
  Zed, …) via MCP — the original phase-1 deliverable.
- **Agent harness** in its own right: interactive TUI, ACP backend for
  Zed, one-shot CLI, and daemon-owned sessions with remote attach — with
  multi-provider routing, model failover, compaction, and per-turn
  checkpoints built in (see [Agent harness
  features](#agent-harness-features)).

**Status: Production-ready.** Both roles are used daily for real development
work — including developing Daimonos itself. Pre-built binaries available
for Linux (x86_64, aarch64, musl) and macOS (Apple Silicon, Intel).

### Phase 2: Minimal Linux distro

A purpose-built Buildroot Linux image with Daimonos as the primary user-space
application. Designed for cloud deployment where AI agents need a clean,
minimal environment. The distro boots in seconds, has no shell or human-facing
UI, and runs the Daimonos daemon as PID 1's direct child. The session daemon
and remote-control gateway from phase 1 are the intended tenants: headless
agent sessions in the cloud, attached to from a terminal or phone.

**Status: Working prototype.** Boots in QEMU, deployable to AWS EC2. Used for
remote benchmarking.

### Phase 3: Custom microkernel

The long-term vision: a microkernel where Daimonos opcodes become native
syscalls. StructFS (a filesystem that stores and returns structured data
natively), capability-based security, and a process model designed for agent
workloads from the ground up.

**Status: Design phase.**

## Development

### Prerequisites

| Dependency | Required | Install |
|-----------|----------|---------|
| Rust (stable 1.75+) | Build | [rustup.rs](https://rustup.rs) |
| Python 3 + pytest | Tests | `pip install -r tests/requirements.txt` |

### Running tests

```bash
# Rust unit tests (350+ tests, parallel-safe)
cargo test

# End-to-end MCP protocol tests (150+ pytest cases)
python3 -m pytest tests/ -v
```

### Running benchmarks

```bash
cd benchmarks
./setup-mcp.sh
./run-benchmark.sh baseline   # IDE built-in tools
./run-benchmark.sh daimonos   # routed through daimonos MCP
python3 analyze-results.py results/
```

See [benchmarks/README.md](benchmarks/README.md) for details.

## Configuration

All behavior is tunable via a TOML config file. See
[docs/configuration.md](docs/configuration.md) for the full reference, or
[daimonos.default.toml](daimonos.default.toml) for annotated defaults.

Key sections:
- `[index]` — Trigram indexer tuning (max depth, file size limits)
- `[search]` — Search result limits
- `[process]` — Process timeouts, in-memory/artifact bounds, background
  admission, termination grace, inherited environment, semantic filters, and
  max concurrent Starlark script threads
- `[pipeline_cache]` — Subprocess result cache size, inotify watch cap, extra ignored directories
- `[analytics]` — Token tracking (SQLite storage, retention)
- `[tools.*]` — Per-tool plugin configuration

## Contributing

Daimonos is in active development. If you're interested in contributing, start
with the [AGENTS.md](AGENTS.md) file for coding conventions, architecture
decisions, and the review checklist.

See also:
- [CONTRIBUTING.md](CONTRIBUTING.md)
- [SECURITY.md](SECURITY.md)
- [CHANGELOG.md](CHANGELOG.md)
- [LICENSE](LICENSE)

## License

MIT
