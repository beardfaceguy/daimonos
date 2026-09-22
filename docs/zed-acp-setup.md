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

## Hosted MCP OAuth

A Zed OAuth grant in Zed's keychain is **not** forwarded to a custom ACP
agent. Daimonos must own a separate OAuth grant for hosted servers such as
`https://mcp.notion.com/mcp`. Configure a non-secret endpoint policy in Daimonos as described in
[configuration.md](configuration.md#acpmcpoauth_serversname--outbound-oauth-policy).
For Notion, use `url = "https://mcp.notion.com/mcp"` and `profile = "meetalix"`.
The native flow requires Zed's `context_servers.notion` entry to use a
**direct remote URL**, not a `command` launching `mcp-remote`:

```jsonc
"notion": { "enabled": true, "url": "https://mcp.notion.com/mcp" }
```

Remove the previous `command`/`args`/`env` fields *from this entry only* when
switching. Keep a backup of Zed settings so the working stdio workaround can
be restored if needed. Do not include an `Authorization` header or remove
Zed's existing keychain grant.

From a terminal, run `daimonos mcp auth login notion`. Daimonos prints the
URL *and* attempts to open your browser. Select **MeetAlix** on Notion's
consent screen. Then run `daimonos mcp auth status notion` and open a new
Daimonos ACP session in Zed (or reconnect the current session) so the bridge
rediscovers `mcp__notion__*` tools. Fetch the roadmap page with a Notion MCP
tool, not web search. If the authenticated tool returns a page-level 404,
check Notion workspace/page permissions separately. Token refresh is automatic;
`daimonos mcp auth logout notion` removes only Daimonos's grant.

Daimonos uses a private loopback proxy for authenticated HTTP, so bearer
tokens stay out of Zed's keychain/config and ACP messages. A missing grant is
reported as `auth_required`; unrelated servers continue to work. No live
Notion authorization or page fetch has been performed by this implementation
work. Do not remove Zed's existing grant. The `mcp-remote` stdio workaround
remains available if this native flow is incompatible with the server.

## Shared MCP configuration

Daimonos treats Zed as the primary MCP configuration by default. Non-ACP
agents read `~/.config/zed/settings.json` directly, including its JSONC
`context_servers`. ACP sessions resolve servers in this order:

1. Servers forwarded by the current ACP harness.
2. If—and only if—ACP initialize identifies the harness as Zed and it forwards
   an empty list, Zed `context_servers`.
3. For that same Zed recovery case, `[agent.mcp].servers_file` as the final
   Daimonos fallback.

An empty list from Cursor or any unknown/non-Zed ACP harness remains
authoritative and produces no MCP servers.

To select a Claude/Cursor-style shared file instead:

```toml
[agent.mcp]
servers_file = "~/.config/mcp/servers.json"
```

The selected file may contain either `mcpServers` or `context_servers`; if it
contains both, `context_servers` takes precedence. A harness that cannot select a path may symlink its standalone MCP file to the
shared file. Do not symlink all of Zed's `settings.json` over a harness file
that only accepts a standalone `mcpServers` document. While Zed's file is being
edited, invalid JSONC causes Daimonos to skip configured MCP servers and emit a
warning rather than failing the agent session.

### Cursor ACP and other file-only harnesses

Cursor ACP currently ignores ACP-forwarded MCP servers and reads
`~/.cursor/mcp.json`, whose required `mcpServers` shape is incompatible with
Zed's full `settings.json`. Use the built-in translator and launch wrapper:

```sh
daimonos mcp-config sync --target ~/.cursor/mcp.json -- cursor-agent acp
```

The command reads Zed at process start, atomically regenerates Cursor's file,
and then replaces itself with Cursor ACP. Configure Zed's `agent_servers.cursor`
as `type = "custom"` with that command/argument sequence if the registry entry
cannot be wrapped. This guarantees each new Cursor ACP sees Zed's MCP list as of launch without
manually copying settings. Changes made while a Cursor session is running take
effect on its next launch. `--dry-run` prints the generated JSON; omit
`-- COMMAND` to perform a one-time sync.

Because the formats differ, a direct symlink is unsafe. The generated file is
mode `0600`; if an old Cursor file exists it is atomically replaced. Disabled
Zed servers and Zed-only metadata such as `timeout` are omitted. The target
is fully owned by synchronization and must not be edited manually; a valid Zed
configuration containing zero enabled servers intentionally produces an empty
Cursor list.

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

## Context compaction (required config)

Long conversations eventually exceed the model's context window. daimonos
can compact them automatically (ADR-002): when the measured prompt size
crosses a high-water mark, the oldest turns are summarized into one message
so the conversation keeps fitting. **The agent env file must configure this
explicitly — there are no defaults in code, and daimonos errors at startup
if the keys are missing:**

```
# Master switch — always required (on or off):
DAIMONOS_AGENT_COMPACTION=on

# Required when on:
DAIMONOS_AGENT_COMPACTION_HIGH_WATER=0.75   # compact when prompt ≥ 75% of budget
DAIMONOS_AGENT_COMPACTION_LOW_WATER=0.50    # evict down to ~50% of budget
DAIMONOS_AGENT_OUTPUT_RESERVATION=8192      # tokens reserved for the reply

# Optional:
DAIMONOS_AGENT_CONTEXT_WINDOW=200000        # your model's window, in tokens;
                                            # omit to resolve it live from the provider (#965)
DAIMONOS_AGENT_SUMMARY_MODEL=anthropic/claude-haiku-4.5  # unset → the main model
DAIMONOS_AGENT_SUMMARY_PROMPT=...                        # unset → built-in template
```

The budget is `CONTEXT_WINDOW − OUTPUT_RESERVATION`; watermarks must satisfy
`0 < LOW < HIGH < 1`. `CONTEXT_WINDOW` is optional (#965): when omitted,
daimonos queries the provider for the effective model's window (OpenRouter
`context_length` / Anthropic `max_input_tokens` / native OpenAI known-model
metadata) and errors out if it can't be
determined. If you use the model picker across models with different windows,
either leave `CONTEXT_WINDOW` unset (each model resolves its own) or set it for
the smallest one. The simplest
valid setup is `DAIMONOS_AGENT_COMPACTION=off` (no other keys needed).
When a compaction happens, Zed shows a collapsed thought line
(`[context compacted: N older turn(s) summarized]`); the chat REPL prints
the equivalent notice.

## Verify

1. Open Zed's agent panel and select **daimonos** as the agent.
2. Send a prompt — the reply should stream in live.
3. Ask it to run a shell command or edit a file — Zed's built-in
   permission-approval UI should prompt before the tool runs.
4. Token/cost usage for the session is shown via Zed's usage indicator.
5. If you set `DAIMONOS_AGENT_MODELS`, the model dropdown at the bottom of
   the chat lists your models; picking one applies to the next message.
6. File `@mentions` include their contents, and pasted images are sent to
   image-capable providers.
7. Foreground `exec` calls show stdout/stderr live in a terminal card, followed
   by the command's exit status.

## Scope

- Multiple concurrent sessions per `daimonos acp` process (Zed keeps one
  process across chat threads), with `session/load` resume — reopening a
  thread after a window switch or a full Zed restart restores its history
  (persisted under `~/.daimonos/acp-sessions`).
- Embedded file context and pasted images are preserved in the model prompt.
  Image capability is advertised only when the configured provider adapter
  supports multimodal requests (the Anthropic and OpenRouter adapters do;
  native OpenAI GPT-5.6 Sol is text-only).
  Audio prompts are not advertised.
- Provider thinking streams as collapsible thought chunks and is restored when
  a saved session is loaded.
- Tool execution and file access are handled entirely by daimonos's own
  tools — the `fs/*`/`terminal/*` client-proxy methods aren't used.
- When Zed advertises its terminal-output metadata extension, foreground
  `exec` runs directly so daimonos can mirror subprocess output live. The
  completed tool result remains structured and still uses semantic output
  filtering and output caps. Other clients retain plugin redirects and the
  existing completion-only behavior.

## Troubleshooting

- Verify the binary works standalone first: `daimonos agent "say hi"`.
- `--debug-tokens` logs per-call token usage to
  `~/.config/daimonos/token-debug.log` if you want to inspect usage
  independent of Zed's own display. Native-agent records also include
  metadata-only context composition (numeric sizes/counts by category); prompt,
  tool, image, path, and provider-state content is never logged. It's a global
  flag, so it must come
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
