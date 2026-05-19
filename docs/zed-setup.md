# Zed Editor Setup

This guide configures Zed to use daimonos as its MCP server.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- Zed editor installed

## Setup

### 1. Open Zed settings

Open Zed settings via **Zed > Settings > Open Settings** (or `Cmd+,` on
macOS).

### 2. Add daimonos as an MCP server

Add the following to your `settings.json` under the `"context_servers"` key:

```json
{
  "context_servers": {
    "daimonos": {
      "command": {
        "path": "daimonos",
        "args": ["--mcp", "-w", "."]
      }
    }
  }
}
```

If daimonos isn't on your PATH, use the absolute path:

```json
{
  "context_servers": {
    "daimonos": {
      "command": {
        "path": "/usr/local/bin/daimonos",
        "args": ["--mcp", "-w", "/absolute/path/to/workspace"]
      }
    }
  }
}
```

### 3. Verify

1. Open the Zed AI assistant panel.
2. Daimonos tools should be available in the context.
3. Ask the assistant to read a file to confirm tools are working.

## Troubleshooting

- Verify the binary path: run `daimonos --help` in your terminal.
- Restart Zed after changing the config.
- Context-aware tools (`cargo`, `git`, `gh`) only appear when their
  prerequisites exist in the workspace.
- **Idle disconnect (~10 min):** `[mcp] idle_timeout_secs` defaults to `600`;
  raise or set `0` in config if you want the MCP server to stay up without
  traffic (see `docs/configuration.md`).
- **Diagnostics:** `--verbose`, `DAIMONOS_LOG_STARTUP=1`, or `[mcp] startup_logs = true`
  restores informational stderr during MCP startup (Cursor treats subprocess stderr as errors;
  Zed is usually quieter — same flags apply).
