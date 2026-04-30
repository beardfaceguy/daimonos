# Cursor IDE Setup

This guide configures Cursor to use daimonos as its MCP server, replacing
built-in file/search/exec tools with agent-optimized equivalents.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- Cursor IDE installed

## Setup

### 1. Create the MCP config file

In your project's root directory, create `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "/absolute/path/to/daimonos",
      "args": ["--mcp", "-w", "/absolute/path/to/your/workspace"]
    }
  }
}
```

If you installed to `/usr/local/bin/` (the default for pre-built binaries),
you can use `daimonos` directly and `.` for the current workspace:

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

If you built from source and didn't install to PATH, use absolute paths:

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "/home/youruser/daimonos/target/release/daimonos",
      "args": ["--mcp", "-w", "/home/youruser/projects/my-app"]
    }
  }
}
```

### 2. Verify in Cursor

1. Open your project in Cursor.
2. Open the MCP panel (Cursor Settings > MCP, or check the MCP icon in the
   status bar).
3. You should see "daimonos" listed as an available server.
4. The server starts automatically when an agent session begins.

### 3. Add the system prompt directive (recommended)

To ensure the agent prefers daimonos tools over built-in equivalents, add a
Cursor rule. Create `.cursor/rules/daimonos.mdc` in your project:

```
---
description: Use daimonos MCP tools for all file, search, exec, and git operations
alwaysApply: true
---

Use daimonos MCP tools, not built-in equivalents.
If your plan requires 2+ tool calls, use execute_script instead — write a
Starlark script that calls the tool functions and sets `result`. This is faster
and cheaper than sequential calls. Only call individual tools when you need
exactly one operation.
```

This is optional — the MCP server already includes this directive in its
`instructions` field, but the Cursor rule reinforces it.

## What Changes

Once configured, the agent will use daimonos tools instead of Cursor's built-in
tools. Here's what maps to what:

| Cursor built-in | Daimonos equivalent | Advantage |
|----------------|--------------------|-----------| 
| `Read` | `read_file` | Content-hash dedup (skips re-reads of unchanged files) |
| `Write` | `write_file` | Structured confirmation |
| `Edit` | `edit_file` | Returns diffs showing what changed |
| `Grep` | `search` | Trigram index for instant results |
| `Shell` | `exec` | Smart output truncation, persistent env/cwd |
| `Glob` | `search` (file mode) | Combined content + file search in one tool |
| Git commands via Shell | `git` | Structured JSON output, single tool call |
| Cargo commands via Shell | `cargo` | Structured diagnostics, parsed test results |

Additional capabilities not available in Cursor built-in tools:

| Tool | What it does |
|------|-------------|
| `snapshot` | Instant workspace checkpoint and rollback |
| `execute_script` | Run multi-step Starlark scripts in one call |
| `batch` | Bundle multiple operations in a single round-trip |
| `gh` | Structured GitHub CLI (PRs, API calls) |
| `docker` | Structured Docker/Compose management |

## Troubleshooting

### "daimonos" doesn't appear in MCP panel

- Check that the binary path in `mcp.json` is correct and the binary exists.
- Make sure the binary is executable: `chmod +x /path/to/daimonos`
- Try running the binary manually to verify it starts:
  ```bash
  echo '{}' | /path/to/daimonos --mcp -w /path/to/workspace
  ```
  You should see JSON-RPC output, not an error.

### Agent still uses built-in tools

- Verify the MCP server shows as "connected" in Cursor's MCP panel.
- Add the `.cursor/rules/daimonos.mdc` rule described above.
- Check that `--strict-mcp-config` is not set unless you intend to disable
  built-in tools entirely.

### "Permission denied" errors

- Daimonos sandboxes all file operations to the workspace root. Paths outside
  the workspace are rejected.
- Symlinks that resolve outside the workspace are also blocked.

### Tools like `cargo` or `git` don't appear

- These tools are context-aware. `cargo` only appears when `Cargo.toml` exists
  in the workspace root. `git` and `gh` only appear when `.git/` exists.
- Run `list_all_tools` to see every available tool, including hidden ones.

## Verifying It Works

Start a new agent session in Cursor and ask it to do something simple:

> Read src/main.rs and summarize it.

In the tool call output, you should see `mcp__daimonos__read_file` instead of
Cursor's built-in `Read`. If you see daimonos tool calls, everything is working.
