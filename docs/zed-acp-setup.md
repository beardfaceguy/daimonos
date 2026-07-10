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

## Model picker

Zed's agent panel shows a model dropdown at the bottom of the chat. To
populate it, list the models you want to choose between in your agent env
file via `DAIMONOS_AGENT_MODELS` (comma-separated). The active model
(`DAIMONOS_AGENT_MODEL`, or a `--model` flag) is always included and starts
selected:

```
DAIMONOS_AGENT_MODEL=anthropic/claude-haiku-4.5
DAIMONOS_AGENT_MODELS=anthropic/claude-haiku-4.5, anthropic/claude-sonnet-4.6, anthropic/claude-opus-4.1
```

Use whatever model identifiers your configured provider expects (for
OpenRouter these are namespaced, e.g. `anthropic/claude-haiku-4.5`).
Selecting a model in the dropdown applies to the next message you send.
If `DAIMONOS_AGENT_MODELS` is unset, the dropdown just shows the single
active model.

## Verify

1. Open Zed's agent panel and select **daimonos** as the agent.
2. Send a prompt — the reply should stream in live.
3. Ask it to run a shell command or edit a file — Zed's built-in
   permission-approval UI should prompt before the tool runs.
4. Token/cost usage for the session is shown via Zed's usage indicator.
5. If you set `DAIMONOS_AGENT_MODELS`, the model dropdown at the bottom of
   the chat lists your models; picking one applies to the next message.

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
  independent of Zed's own display. It's a global flag, so it must come
  **before** the `acp` subcommand in `args` (unlike `--model`/`--agent-env`,
  which are `acp` subcommand options and come after):

  ```json
  {
    "agent_servers": {
      "daimonos": {
        "type": "custom",
        "command": "daimonos",
        "args": ["--debug-tokens", "acp"]
      }
    }
  }
  ```
