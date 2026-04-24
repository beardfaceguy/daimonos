# AGENTS.md

Cross-tool guidance for AI coding agents working in this repository.

## Quick start (new agents)

1. Read this file — project context, repo layout, conventions
2. Check the **Daimonos** initiative in Linear for the vision, roadmap, and
   active projects: search for initiative "Daimonos" or use `get_initiative`
3. Check `.cursor/rules/` if scoped rule files exist
4. Check `docs/` for technical specs (protocol, architecture decisions)

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
| Protocol specification | `docs/protocol.md` |
| Architecture decisions | `docs/` directory |
| Coding rules and conventions | This file + `.cursor/rules/` |
| Configuration reference | `daimonos.default.toml` |

Technical documentation that doesn't fit in Linear (specs, diagrams,
benchmarks, design docs) goes in `docs/`. Use Linear documents for research
findings, status reports, and narrative context. Use `docs/` for anything an
agent or developer needs while reading or writing code.

## Repo layout

```
daimonos/
├── AGENTS.md                      # This file
├── Cargo.toml                     # Rust project manifest
├── daimonos.default.toml          # Reference config with all tunable values
├── docs/
│   └── protocol.md                # Opcode protocol specification
└── src/
    ├── main.rs                    # Daemon entrypoint, Unix socket listener
    ├── config.rs                  # TOML config loading, tool registration
    ├── protocol.rs                # Request/Response types, opcode constants
    ├── session.rs                 # Per-connection session state
    ├── index.rs                   # Background trigram workspace indexer
    ├── pipeline_cache.rs          # inotify-based tool output cache
    ├── tool_runner.rs             # ToolPlugin trait, registry, repair loop
    ├── ops/
    │   ├── mod.rs                 # Opcode dispatcher
    │   ├── file_ops.rs            # Opcodes 0-6: read, write, patch, ls, stat, glob, grep
    │   ├── exec_ops.rs            # Opcodes 8-11: exec, bg, poll, kill
    │   ├── tool_ops.rs            # Opcodes 20-24: tool_run, repair, pipeline, register, list
    │   └── schema.rs              # Opcode 255: self-describing schema registry
    └── plugins/
        ├── mod.rs                 # Plugin module
        ├── generic_cli.rs         # Generic plugin for any CLI tool with JSON output
        └── x07.rs                 # X07-specific plugin (semantic indexing, decl cache)
```

## Prerequisites

| Dependency | Required by | Install |
|-----------|-------------|---------|
| Rust (stable) | Building daimonos | `rustup` |
| `socat` | Manual testing via Unix socket | system package manager |

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

## Testing

```bash
# Build
cargo build --release

# Start daemon
./target/release/daimonos --workspace /path/to/workspace --debug

# Send a request (from another terminal)
echo '{"c":255}' | socat - UNIX-CONNECT:/tmp/daimonos.sock

# Batch request
echo '{"batch":[{"c":4,"p":"Cargo.toml"},{"c":6,"p":"fn main","n":5}]}' | \
  socat - UNIX-CONNECT:/tmp/daimonos.sock
```

## Review checklist

When reviewing or producing a diff:

- **Config**: no hardcoded strings that could change (extensions, paths,
  limits). These belong in `daimonos.default.toml`.
- **Protocol**: new opcodes must be added to all three places: `protocol::op`,
  dispatcher in `ops/mod.rs`, schema in `ops/schema.rs`.
- **Responses**: must be structured JSON. No raw text output.
- **Error handling**: no panics, no unwrap() in request paths. Use
  `Response::err()`.
- **Dependencies**: flag new crate additions for confirmation.
