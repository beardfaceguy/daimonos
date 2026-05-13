# Claude Code CLI Setup

This guide configures the Claude Code CLI (`claude`) to use daimonos as its
MCP server for terminal-based agent workflows.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- Claude Code CLI installed (`claude` command available)

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

The Claude CLI reads the same `.cursor/mcp.json` format as Cursor IDE.

### 2. Run with daimonos

Two modes:

**A. Daimonos-only (recommended for benchmarking / clean tests).** Disables
built-in `Read`/`Edit`/`Bash`/`Grep`/etc. so only daimonos tools are
available:

```bash
claude --mcp-config .cursor/mcp.json \
       --strict-mcp-config \
       --tools "" \
       --append-system-prompt "Use daimonos MCP tools, not built-in equivalents."
```

**B. Daimonos alongside built-ins (recommended for daily use).** Exposes
daimonos tools *and* Claude's built-ins; the system prompt nudges the model
to prefer daimonos:

```bash
claude --mcp-config .cursor/mcp.json \
       --append-system-prompt "Use daimonos MCP tools, not built-in equivalents."
```

For non-interactive (piped) usage, add `-p`:

```bash
echo "Read src/main.rs and summarize it" | \
  claude -p \
    --mcp-config .cursor/mcp.json \
    --append-system-prompt "Use daimonos MCP tools, not built-in equivalents."
```

### 3. Convenience alias (optional)

Add to your shell profile (`~/.bashrc`, `~/.zshrc`, etc.):

```bash
alias dclaude='claude --mcp-config .cursor/mcp.json --append-system-prompt "Use daimonos MCP tools, not built-in equivalents. If your plan requires 2+ tool calls, use execute_script to run them as a single Starlark script. Only call individual tools for single-operation tasks."'
```

Then use `dclaude` anywhere:

```bash
dclaude "Run the tests and tell me if anything fails"
```

## Flags Reference

| Flag | Purpose |
|------|---------|
| `--mcp-config <path>` | Path to MCP server config (JSON) |
| `--strict-mcp-config` | Use *only* the MCP servers from `--mcp-config`, ignoring `~/.claude.json` and other sources |
| `--tools ""` | Disable all built-in tools (use with `--strict-mcp-config` for a daimonos-only environment). Pass tool names like `"Bash,Edit"` to allow a subset |
| `--append-system-prompt <text>` | Add instruction to prefer daimonos tools |
| `--model <name>` | Model to use (default: claude-sonnet) |
| `-p` | Pipe mode (read prompt from stdin, no interactive session) |
| `--output-format stream-json` | Machine-readable output (for scripting) |
| `--dangerously-skip-permissions` | Skip tool permission prompts (for CI/automation) |

## Starlark Scripts

Daimonos includes an embedded Starlark interpreter. When the agent needs
multiple operations, it can write a single script instead of making sequential
tool calls:

```python
# Agent writes this as one execute_script call instead of 3 separate calls
content = read_file("src/main.rs")
matches = search(pattern="fn main", path="src/")
status = git("status")
result = {"content": content, "matches": matches, "status": status}
```

The system prompt directive tells the agent to prefer this pattern. Available
functions in Starlark scripts:

```
read_file(path, offset=None, limit=None)
write_file(path, content)
edit_file(path, edits)
search(pattern, mode="content", path=None, glob=None, max_results=None)
exec(command, args=[], cwd=None)
ls(path=None, depth=None)
snapshot(action, id=None, tag=None)
git(command)
cargo(command, package=None, filter=None, ...)
gh(command, number=None, state=None, limit=None, ...)
docker(command, container=None, tail=None, ...)
```

## Verifying It Works

Confirm Claude can see and load daimonos:

```bash
echo "List files in this workspace." | \
  claude -p --mcp-config .cursor/mcp.json \
    --strict-mcp-config --tools "" \
    --append-system-prompt "Use daimonos MCP tools, not built-in equivalents." \
    --output-format stream-json --verbose 2>/dev/null | \
  grep -oE '"mcp_servers":\[[^]]*\]|mcp__daimonos__[a-z_]+' | sort -u
```

Expected output: `"mcp_servers":[{"name":"daimonos","status":"connected"}]`
plus one or more `mcp__daimonos__*` tool names that Claude actually called.

If you see `"status":"failed"` or no `mcp__daimonos__*` lines, common causes:

- The `command` path in `.cursor/mcp.json` is not absolute or not executable.
- The `-w` workspace path is not absolute or doesn't exist.
- The daimonos binary segfaults on startup — run it directly:
  `./daimonos --mcp -w /path/to/workspace` and make sure it stays alive
  reading from stdin.

## Benchmarking

The `benchmarks/` directory includes a full benchmark harness for comparing
daimonos vs built-in tools:

```bash
cd benchmarks

# Set up MCP config for the benchmark workspace
bash setup-mcp.sh

# Run baseline (built-in tools)
bash run-benchmark.sh baseline

# Run daimonos
bash run-benchmark.sh daimonos

# Compare results
python3 analyze-results.py results
```

See `benchmarks/README.md` for details.
