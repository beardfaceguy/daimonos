# Herdr Setup

This guide covers running the daimonos agent frontends inside
[herdr](https://herdr.dev), the terminal runtime that supervises coding
agents in persistent PTY panes.

Unlike the other guides in this directory, herdr does not consume daimonos as
an MCP server — it supervises the `daimonos chat` and `daimonos agent`
frontends as terminal processes and tracks whether each pane is **working**,
**blocked**, or **idle**.

## How the integration works

Herdr's native agent support (process auto-detection, bundled screen-detection
manifests, automatic session restore) requires changes to the herdr binary
itself. Daimonos instead uses herdr's documented
[custom-agent contract](https://herdr.dev/docs/integrations/#integrate-your-own-agent):
a process inside a herdr pane inherits `HERDR_ENV=1`, `HERDR_PANE_ID`, and
`HERDR_BIN_PATH`, and reports its own semantic state through the herdr CLI.

When those variables are present, the `chat` and one-shot `agent` frontends
report (`src/herdr.rs`):

| Moment | Reported state |
|--------|----------------|
| REPL waiting at the `*D*>` prompt | `idle` |
| A turn is in flight | `working` |
| A `[safety]` approval prompt is pending | `blocked` (message names the tool) |
| Frontend exit | `pane release-agent` (authority handed back) |

Reports go through `$HERDR_BIN_PATH pane report-agent $HERDR_PANE_ID --source
custom:daimonos ...` with a monotonically increasing `--seq`. Because the
state comes from lifecycle hooks rather than screen scraping, herdr treats it
as authoritative for the pane.

Outside herdr (no `HERDR_ENV=1`), the integration is a complete no-op.

## Prerequisites

- Daimonos binary installed — [build from source](install.md)
- Herdr installed: `curl -fsSL https://herdr.dev/install.sh | sh`
- Agent env file configured (`~/.config/daimonos/agent.env`) with a provider
  API key — the `chat`/`agent` modes need one

## Setup

No configuration is required. Start herdr where the work lives, then run a
daimonos frontend in any pane:

```bash
herdr          # start / reattach the herdr session
daimonos chat  # inside a pane
```

The pane's status marker follows the conversation: working while a turn runs,
blocked while a `[safety]` approval waits for your `Y`/`y`/`N`, idle at the
prompt.

## Prompting from other agents

Other agents (or you, from scripts) can drive the pane through herdr:

```bash
herdr agent prompt <pane> "summarize the failing tests"
```

Herdr delivers prompts as a bracketed paste followed by Enter; the chat REPL
enables bracketed paste, so multiline prompts arrive as a single submission.
Herdr refuses to prompt a pane whose agent is `blocked` — clear the approval
prompt first (`herdr agent send-keys`).

## Session resume

Every state report carries the chat session id (`--agent-session-id`), so
herdr surfaces it through its pane and agent APIs. Automatic restore after a
herdr server restart only works for herdr's officially integrated agents;
for daimonos, resume manually:

```bash
daimonos chat --list           # find the session id
daimonos chat --resume <id>    # continue where the pane left off
```

## Limitations

- `daimonos agent --interactive` (the full-screen TUI) does not report state
  yet — prefer `daimonos chat` under herdr.
- Automatic session restore on herdr restart requires native herdr support
  (an upstream change to the herdr binary), not just this integration.
