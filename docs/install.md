# Installing Daimonos

Daimonos is a single Rust binary that acts as an MCP server for AI coding
agents. It replaces built-in file, search, exec, and git tools with
agent-optimized equivalents that use fewer tokens and fewer round-trips.

**Supported platforms:** Linux (x86_64, aarch64) and macOS (Apple Silicon,
Intel). Windows is not yet supported.

## Prerequisites

For pre-built binaries: just `curl` and a supported platform (see below).

For building from source:

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

## Option A: Download Pre-Built Binary

Pre-built binaries are available from
[GitHub Releases](https://github.com/beardfaceguy/daimonos/releases).

**Linux:**

```bash
# x86_64 (most Ubuntu/Debian desktops and servers)
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-x86_64-linux.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/

# aarch64 / ARM64 (Graviton, Raspberry Pi, ARM servers)
curl -L https://github.com/beardfaceguy/daimonos/releases/latest/download/daimonos-aarch64-linux.tar.gz | tar xz
sudo mv daimonos /usr/local/bin/

# musl static binary (works on any Linux, no glibc dependency)
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

Verify: `daimonos --help`

## Option B: Build from Source

```bash
# 1. Clone the repo
git clone https://github.com/beardfaceguy/daimonos.git
cd daimonos

# 2. Build (release mode recommended)
cargo build --release

# 3. Verify the binary works
./target/release/daimonos --help
```

To put it on `PATH` instead of referencing `target/release` from your editor
config:

```bash
cargo install --path . --force
# installs to ~/.cargo/bin/daimonos
```

Note that `cargo install` re-resolves dependencies from scratch and ignores
`Cargo.lock`. Pass `--locked` if you want the exact dependency set this repo was
tested against:

```bash
cargo install --path . --force --locked
```

You should see:

```
Daimonos — agent-optimized OS layer

Usage: daimonos [OPTIONS] [COMMAND]

Commands:
  agent   Run the agent on a one-shot task and exit
  chat    Start an interactive chat REPL over a stateful agent session
  acp     Run a native Agent Client Protocol engine over stdio
  mcp     Run the MCP tool server over stdio or a Unix socket
  daemon  Run the compact opcode protocol daemon over a Unix socket

Options:
  -s, --socket <SOCKET>        Unix socket path used by daemon mode [default: /tmp/daimonos.sock]
  -w, --workspace <WORKSPACE>  Workspace root directory [default: .]
      --debug                  Human-readable debug output (daemon mode only)
  -c, --config <CONFIG>        Path to config file (default: search workspace then ~/.config/daimonos/)
      --mcp                    Legacy alias for `daimonos mcp`
      --mcp-socket <PATH>      Legacy alias for `daimonos mcp --socket <PATH>`
  -h, --help                   Print help
```

See [runtime modes](runtime-modes.md) for recommended invocations and
compatibility details.

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

For repository agent/operator conventions, see `AGENTS.md` (especially
**Daimonos tool usage policy**).

Pick the setup guide for your AI tool:

- [Cursor IDE](cursor-setup.md)
- [GitHub Copilot](copilot-setup.md) (VS Code, Visual Studio, JetBrains, Xcode, Eclipse)
- [Claude Code](claude-code-setup.md) (CLI + macOS Desktop app)
- [Windsurf](windsurf-setup.md)
- [Cline](cline-setup.md) (VS Code extension)
- [Gemini CLI](gemini-cli-setup.md)
- [Zed Editor](zed-setup.md)
- [Discord integration](discord-setup.md) (bot token, allowlists, read-only tools)
- [Other tools](other-tools-setup.md) (Claude Desktop, ChatGPT, Continue.dev, BoltAI, etc.)

For tuning behavior: [Configuration reference](configuration.md)
