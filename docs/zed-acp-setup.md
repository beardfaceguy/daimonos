# Zed Editor Setup (native ACP agent)

This guide configures Zed to use daimonos as a native **Agent Client
Protocol** (ACP) agent — no MCP adapter in between. For the MCP-based
setup (context tools inside an existing assistant), see
[zed-setup.md](zed-setup.md) instead; the two are independent and can be
used together.

## Prerequisites

- Daimonos binary installed — [download a pre-built binary](https://github.com/beardfaceguy/daimonos/releases) or [build from source](install.md)
- A working agent env file (see `docs/configuration.md` / `daimonos agent --help`) —
  `daimonos acp` loads it the same way `daimonos agent`/`daimonos chat` do
- Zed editor installed, with agent-panel support for custom `agent_servers`

## Setup

Add daimonos under the `agent_servers` key in Zed's `settings.json`. The
`"type": "custom"` field is required — Zed tags each agent-server entry by
type, and omitting it produces a `Missing property "type"` error:

```json
{
  "agent_servers": {
    "daimonos": {
      "type": "custom",
      "command": "daimonos",
      "args": ["acp"]
    }
  }
}
```

If daimonos isn't on your `PATH`, use the absolute path:

```json
{
  "agent_servers": {
    "daimonos": {
      "type": "custom",
      "command": "/usr/local/bin/daimonos",
      "args": ["acp"]
    }
  }
}
```

To pin a specific model/provider or agent env file, pass the same flags
`daimonos agent`/`daimonos chat` accept:

```json
{
  "agent_servers": {
    "daimonos": {
      "type": "custom",
      "command": "daimonos",
      "args": ["acp", "--model", "claude-opus-4-8", "--agent-env", "/path/to/agent.env"]
    }
  }
}
```

## Verify

1. Open Zed's agent panel and select **daimonos** as the agent.
2. Send a prompt — the reply should stream in live.
3. Ask it to run a shell command or edit a file — Zed's built-in
   permission-approval UI should prompt before the tool runs.
4. Token/cost usage for the session is shown via Zed's usage indicator.

## Scope (v1)

- One active session per `daimonos acp` process (Zed spawns a fresh
  process per agent connection, so this matches normal usage).
- Text-only prompts (image/audio/resource content blocks are ignored).
- No `session/load` (resuming a previous session) yet.
- Tool execution and file access are handled entirely by daimonos's own
  tools — the `fs/*`/`terminal/*` client-proxy methods aren't used.

## Troubleshooting

- Verify the binary works standalone first: `daimonos agent "say hi"`.
- `--debug-tokens` logs per-call token usage to
  `~/.config/daimonos/token-debug.log` if you want to inspect usage
  independent of Zed's own display. Pass it in `args`, same as any other
  flag:

  ```json
  {
    "agent_servers": {
      "daimonos": {
        "type": "custom",
        "command": "daimonos",
        "args": ["acp", "--debug-tokens"]
      }
    }
  }
  ```
