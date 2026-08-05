# Daimonos — Installation & Setup

Daimonos is a single Rust binary that acts as an MCP server for AI coding
agents. It replaces built-in file, search, exec, and git tools with
agent-optimized equivalents that use fewer tokens and fewer round-trips.
Drop it into Cursor or Claude Code and your agent gets structured I/O,
Starlark scripting, native cargo/gh/docker tools, and workspace snapshots
— all in one process, no dependencies.

## Supported Platforms

| Platform | Architecture | Pre-built binary | Build from source |
|----------|-------------|-----------------|-------------------|
| Linux (Ubuntu/Debian) | x86_64 | Yes | Yes |
| Linux (Ubuntu/Debian) | aarch64 | Yes | Yes |
| Linux (any distro) | x86_64 | Yes (musl static) | Yes |
| macOS | Apple Silicon (aarch64) | Yes | Yes |
| macOS | Intel (x86_64) | Yes | Yes |

## Quick Start

### Option A: Pre-Built Binary (recommended)

Pre-built binaries are available from
[GitHub Releases](https://github.com/beardfaceguy/daimonos/releases).
No Rust toolchain needed.

**Linux:**

```bash
# x86_64 (most desktops and servers)
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-x86_64-linux.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/

# aarch64 / ARM64 (Graviton, Raspberry Pi)
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-aarch64-linux.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/

# musl static binary (any Linux, no glibc dependency)
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-x86_64-linux-musl.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/
```

**macOS:**

```bash
# Apple Silicon (M1/M2/M3/M4)
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-aarch64-macos.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/

# Intel Mac
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-x86_64-macos.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/
```

Verify the install:

```bash
daimonos --help
```

### Option B: Build from Source

Requires Rust 1.75+ (stable), Git, and a C compiler.

```bash
# 1. Clone the repository
git clone https://github.com/beardfaceguy/daimonos.git
cd daimonos

# 2. Build
cargo build --release

# 3. (Optional) Install to PATH
sudo cp target/release/daimonos /usr/local/bin/

# 4. Verify
daimonos --help
```

First build takes 1-2 minutes. Subsequent builds are incremental (~5 seconds).

## Configure Your IDE

Once installed, set up Daimonos with your editor:

- **[Cursor IDE](docs/cursor-setup.md)** — recommended for most users
- **[Claude Code](docs/claude-code-setup.md)** — CLI (Linux/macOS/WSL) + macOS Desktop app

The minimal Cursor config (add to `.cursor/mcp.json` in your project):

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "daimonos",
      "args": ["--mcp", "-w", "."]
    }
  }
}
```

## Environment variables (`agent.env`)

Daimonos loads an optional dotenv-style **`agent.env`** at startup — before it
loads config and before it picks a run mode — so the values apply uniformly to
**all** modes (`--mcp`, `agent`, `chat`, `acp`). Search order (project-local
wins over user-global; the real environment always wins over both):

1. `<workspace>/agent.env`
2. `$XDG_CONFIG_HOME/daimonos/agent.env` (else `~/.config/daimonos/agent.env`)

```bash
# <workspace>/agent.env
DAIMONOS_AGENT_AUTO_CONTINUE=3
ANTHROPIC_API_KEY=sk-...
```

A variable already set in the process environment (e.g. Zed's ACP server `env`
block) is never overwritten. For safety, `agent.env` sits in an untrusted
checkout yet daimonos' environment is inherited by every tool it runs, so
loader/interpreter-hijacking variables (`LD_PRELOAD`, `DYLD_*`, `PATH`,
`NODE_OPTIONS`, `BASH_ENV`, …) are **refused** from the file and reported on
stderr; set those in the real environment if you truly need them. Keep
`agent.env` out of version control (add it to `.gitignore`).

## What You Get

| Feature | Benefit |
|---------|---------|
| **Structured tool output** | JSON responses instead of raw text — fewer tokens, better parsing |
| **Starlark scripting** | Batch multiple tool calls in one round-trip via `execute_script` |
| **Native plugins** | `cargo`, `gh`, `docker`, `git` as first-class MCP tools |
| **Workspace snapshots** | Save/restore workspace state for safe experimentation |
| **Terse output directive** | Automatic prompt tuning for ~27% output token reduction |
| **Read deduplication** | Re-reads of unchanged files return a compact "unchanged" response |
| **Trigram file search** | Fast file-name search without shelling out to `find` |

## Optional Dependencies

These are auto-detected at startup. If present, they're exposed as native
MCP tools with structured output:

| Tool | Enables |
|------|---------|
| `cargo` | `cargo` tool (test, build, check, clippy, fmt) |
| `gh` | GitHub CLI tool (pr_list, pr_create, api, etc.) |
| `docker` | Docker tool (ps, logs, images, compose, etc.) |

## Further Reading

- [Configuration reference](docs/configuration.md) — tuning indexing, search, exec
- [Protocol specification](docs/protocol.md) — the opcode protocol under the hood
- [Benchmark results](benchmarks/README.md) — token savings measurements
