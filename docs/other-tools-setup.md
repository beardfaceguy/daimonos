# Other MCP-Compatible Tools

Daimonos works with any AI tool that supports the Model Context Protocol (MCP)
over stdio transport. This guide covers the general pattern and specific notes
for tools not covered by the dedicated setup guides.

## General Pattern

Most MCP-compatible tools use a JSON configuration with the same structure:

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

- `command`: path to the daimonos binary (or just `daimonos` if on PATH)
- `args`: always `["--mcp", "-w", "<workspace_path>"]`
- `.` for current directory, or an absolute path for a specific workspace

The config file location varies by tool — check your tool's MCP documentation.

## Claude Desktop

Config file: `~/.config/claude/claude_desktop_config.json` (Linux) or
`~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "daimonos",
      "args": ["--mcp", "-w", "/path/to/workspace"]
    }
  }
}
```

Note: Claude Desktop requires an absolute workspace path since there's no
"project" concept.

## Continue.dev (VS Code / JetBrains)

Config file: `~/.continue/config.json`

Add daimonos under the `"mcpServers"` key:

```json
{
  "mcpServers": [
    {
      "name": "daimonos",
      "command": "daimonos",
      "args": ["--mcp", "-w", "."]
    }
  ]
}
```

## ChatGPT Desktop

ChatGPT supports MCP connectors. Add daimonos through the MCP settings
in the ChatGPT desktop app preferences.

## BoltAI

BoltAI has built-in MCP quick setup. Look for the MCP servers section
in BoltAI preferences and add daimonos with the standard command/args.

## AnythingLLM

AnythingLLM supports all MCP tools via its agent configuration. Add
daimonos as an MCP server in the agent settings.

## LibreChat

LibreChat supports MCP for multi-user environments. Add daimonos to
the MCP configuration in the LibreChat admin settings.

## Android Studio (Gemini Agent)

Navigate to **File > Settings > Tools > AI > MCP Servers** and add:

- **Command**: `daimonos`
- **Args**: `--mcp -w /path/to/workspace`

## Tips

### Workspace path

- Use `.` when the tool supports project-relative paths (Cursor, VS Code,
  JetBrains, Zed).
- Use absolute paths for tools that don't have a project concept (Claude
  Desktop, standalone CLIs).

### System prompt

Most tools let you add a system prompt or custom instructions. Adding
this directive improves tool usage:

```
Use daimonos MCP tools for all file, search, exec, and git operations.
If your plan requires 2+ tool calls, use execute_script instead — write a
Starlark script that calls the tool functions and sets `result`.
```

Daimonos already includes this in its MCP `instructions` field, but some
tools benefit from reinforcement.

### Verifying the connection

Run a simple test like "Read README.md" and check that the response comes
from a daimonos tool call rather than the tool's built-in file reader.
