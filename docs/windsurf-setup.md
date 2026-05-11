# Windsurf Setup

This guide configures Windsurf to use daimonos as its MCP server.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- Windsurf IDE installed

## Setup

### 1. Open MCP configuration

In Windsurf, open the command palette (`Ctrl+Shift+P` / `Cmd+Shift+P`) and
search for **"MCP"** or navigate to **Settings > MCP Servers**.

### 2. Add daimonos as an MCP server

Add the following to your MCP configuration:

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

If daimonos isn't on your PATH, use the absolute path to the binary.

### 3. Verify

1. Open Windsurf's AI chat panel.
2. You should see daimonos tools available in the tool list.
3. Ask the agent to read a file — it should use `daimonos.read_file` instead
   of Windsurf's built-in tools.

## Adding a Rules Directive (optional)

To reinforce that the agent should prefer daimonos tools, create a
`.windsurfrules` file in your project root:

```
Use daimonos MCP tools for all file, search, exec, and git operations.
If your plan requires 2+ tool calls, use execute_script instead.
```

## Troubleshooting

- If tools don't appear, verify the binary path and restart Windsurf.
- Context-aware tools (`cargo`, `git`, `gh`) only appear when their
  prerequisites exist in the workspace (`Cargo.toml`, `.git/`).
