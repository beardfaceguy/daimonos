# Installing Daimonos

Daimonos is a single Rust binary that acts as an MCP server for AI coding
agents. It replaces built-in file, search, exec, and git tools with
agent-optimized equivalents that use fewer tokens and fewer round-trips.

## Prerequisites

| Dependency | Version | Why |
|-----------|---------|-----|
| **Rust** (stable) | 1.75+ | Builds the daimonos binary |
| **Git** | any | Required for the `git` tool plugin |

Optional (auto-detected at startup):

| Dependency | Enables |
|-----------|---------|
| `cargo` | Native `cargo` tool (test, build, check, clippy, fmt) |
| `gh` | Native GitHub CLI tool (pr_list, pr_create, api, etc.) |
| `docker` | Native Docker tool (ps, logs, images, compose, etc.) |
| `node` | Used internally for some benchmark tooling |

## Quick Start

```bash
# 1. Clone the repo
git clone https://github.com/your-org/daimonos.git
cd daimonos

# 2. Build (release mode recommended)
cargo build --release

# 3. Verify the binary works
./target/release/daimonos --help
```

You should see:

```
Daimonos — agent-optimized OS layer

Usage: daimonos [OPTIONS]

Options:
  -s, --socket <SOCKET>        Unix socket path (ignored in --mcp mode) [default: /tmp/daimonos.sock]
  -w, --workspace <WORKSPACE>  Workspace root directory [default: .]
      --debug                  Human-readable debug output (socket mode only)
  -c, --config <CONFIG>        Path to config file (default: search workspace then ~/.config/daimonos/)
      --mcp                    Run as MCP server over stdio (for Cursor integration)
  -h, --help                   Print help
```

## Build Notes

**First build** takes 1-2 minutes to compile dependencies. Subsequent builds
are incremental and take a few seconds.

**macOS users**: works on both Intel and Apple Silicon. No extra flags needed.

**Linux users**: works on any distro with a recent glibc. Tested on Ubuntu
22.04+ and Fedora 39+.

## What Gets Built

The build produces a single binary at `target/release/daimonos` (~15 MB).
There is no runtime, no daemon to manage, no background service. The binary
starts when your IDE or CLI launches it and exits when the session ends.

## Next Steps

- [Set up with Cursor IDE](cursor-setup.md) — recommended for most users
- [Set up with Claude Code CLI](claude-cli-setup.md) — for terminal-based workflows
- [Configuration reference](configuration.md) — tuning indexing, search, and exec behavior
