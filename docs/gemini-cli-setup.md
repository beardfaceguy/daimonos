# Gemini CLI Setup

This guide configures the Gemini CLI to use daimonos as its MCP server.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- Gemini CLI installed

## Setup

### 1. Edit Gemini CLI settings

Open or create the Gemini CLI settings file at `~/.gemini/settings.json`
and add daimonos as an MCP server:

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

For a specific workspace, use an absolute path:

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "/usr/local/bin/daimonos",
      "args": ["--mcp", "-w", "/home/user/projects/my-app"]
    }
  }
}
```

### 2. Run Gemini CLI

Start a Gemini CLI session. Daimonos tools will be available alongside
Gemini's built-in capabilities:

```bash
gemini
```

### 3. Verify

Ask Gemini to use a daimonos tool:

> Read src/main.rs and summarize it.

You should see daimonos tool calls in the output.

## Troubleshooting

- Verify the binary path: run `daimonos --help` in your terminal.
- Check that `~/.gemini/settings.json` is valid JSON.
- Context-aware tools (`cargo`, `git`, `gh`) only appear when their
  prerequisites exist in the workspace.
