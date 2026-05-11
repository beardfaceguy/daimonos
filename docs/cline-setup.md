# Cline Setup

This guide configures Cline (VS Code extension) to use daimonos as its
MCP server.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- Cline extension installed in VS Code

## Setup

### 1. Open MCP settings

In VS Code with Cline installed, open the Cline sidebar panel and click the
**MCP Servers** icon (plug icon) at the top.

### 2. Add daimonos

Click **"Configure MCP Servers"** and add the following to the configuration:

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

1. Open a Cline chat session.
2. The MCP tools icon should show daimonos tools available.
3. Ask Cline to read a file — it should use daimonos tools.

## Custom Instructions (optional)

In Cline's settings, you can add custom instructions to prefer daimonos:

```
Use daimonos MCP tools for all file, search, exec, and git operations.
If your plan requires 2+ tool calls, use execute_script instead.
```

## Troubleshooting

- If tools don't appear, check the MCP Servers panel for connection errors.
- Try restarting the MCP server from the panel.
- Context-aware tools (`cargo`, `git`, `gh`) only appear when their
  prerequisites exist in the workspace.
