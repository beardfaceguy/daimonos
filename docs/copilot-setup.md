# GitHub Copilot Setup

This guide configures GitHub Copilot to use daimonos as an MCP server in
VS Code, Visual Studio, JetBrains IDEs, Xcode, and Eclipse.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- GitHub Copilot subscription (Free, Pro, Pro+, Business, or Enterprise)
- For Business/Enterprise: the "MCP servers in Copilot" policy must be enabled
  by your org admin

## VS Code Setup

### 1. Create the MCP config file

In your project root, create `.vscode/mcp.json`:

```json
{
  "servers": {
    "daimonos": {
      "command": "daimonos",
      "args": ["--mcp", "-w", "."]
    }
  }
}
```

If daimonos isn't on your PATH, use the absolute path:

```json
{
  "servers": {
    "daimonos": {
      "command": "/usr/local/bin/daimonos",
      "args": ["--mcp", "-w", "/absolute/path/to/workspace"]
    }
  }
}
```

### 2. Start the server

Open the `.vscode/mcp.json` file in VS Code. A **Start** button appears above
the server list. Click it to start daimonos.

### 3. Use in Copilot Chat

1. Open Copilot Chat (click the Copilot icon in the title bar).
2. Select **Agent** from the mode dropdown.
3. Click the tools icon to verify daimonos tools are listed.
4. Ask Copilot to do something — it will use daimonos tools automatically.

## Visual Studio Setup

1. Open Copilot Chat: **View > GitHub Copilot Chat**.
2. Select **Agent** mode from the dropdown.
3. Click the tools icon, then the **+** icon.
4. Fill in:
   - **Server ID**: `daimonos`
   - **Type**: `stdio`
   - **Command**: `daimonos`
   - **Args**: `--mcp`, `-w`, `/path/to/workspace`
5. Click **Save**.

The resulting `mcp.json`:

```json
{
  "servers": {
    "daimonos": {
      "type": "stdio",
      "command": "daimonos",
      "args": ["--mcp", "-w", "/path/to/workspace"]
    }
  }
}
```

## JetBrains IDEs Setup

Works with IntelliJ IDEA, PyCharm, WebStorm, GoLand, RustRover, CLion,
PhpStorm, RubyMine, DataGrip, Rider, and Android Studio.

1. Open Copilot Chat from the status bar.
2. Switch to **Agent** mode, then click "Configure your MCP server".
3. Click **Add MCP Tools**.
4. Add to the `mcp.json`:

```json
{
  "servers": {
    "daimonos": {
      "command": "daimonos",
      "args": ["--mcp", "-w", "."]
    }
  }
}
```

## Xcode Setup

1. Open Copilot Chat in Xcode.
2. Click the gear icon > **MCP** tab > **Edit Config**.
3. Add to `mcp.json`:

```json
{
  "servers": {
    "daimonos": {
      "command": "daimonos",
      "args": ["--mcp", "-w", "."]
    }
  }
}
```

## Shared Config with Cursor

If you already have daimonos configured for Cursor (`.cursor/mcp.json`),
VS Code can auto-discover it. Add to your VS Code `settings.json`:

```json
"chat.mcp.discovery.enabled": true
```

## Verifying It Works

In Copilot Chat (Agent mode), ask:

> Read src/main.rs and summarize it.

You should see daimonos tools being called instead of Copilot's built-in
file reading. Check the tools icon to confirm daimonos tools are active.

## Troubleshooting

### Tools don't appear in Copilot Chat

- Make sure you're in **Agent** mode, not Ask or Edit mode.
- Verify the binary path is correct: run `daimonos --help` in your terminal.
- Check that the MCP server shows as running (VS Code: open `.vscode/mcp.json`
  and look for the "Running" status).

### "MCP servers in Copilot" policy error

- This affects Business/Enterprise plans only. Ask your org admin to enable
  the policy in GitHub Copilot settings.
- Free, Pro, and Pro+ plans are not affected by this policy.

### Context-aware tools missing

- `cargo` only appears when `Cargo.toml` exists in the workspace.
- `git` and `gh` only appear when `.git/` exists.
- Run `list_all_tools` to see every available tool.
