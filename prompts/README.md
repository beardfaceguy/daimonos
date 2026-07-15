# Daimonos prompts

These files are the **model-facing prompts** that steer daimonos's behavior.
They are not documentation — their exact text is sent to the LLM.

## How they are used

Each file here is embedded into the binary at compile time (`include_str!`)
as the built-in default. At runtime you can override any of them **without
recompiling** by pointing the matching key in your `daimonos.toml` at a file:

```toml
[prompts]
# agent_system    = "~/.config/daimonos/prompts/agent_system.md"
# mcp_instructions = "~/.config/daimonos/prompts/mcp_instructions.md"
# kgl_hint         = "~/.config/daimonos/prompts/kgl_hint.md"
# summary          = "~/.config/daimonos/prompts/summary.md"
```

An unset (or empty) key uses the embedded default. A key that points at an
**unreadable** file falls back to the embedded default and logs a warning on
stderr (it does not crash).

`~` is expanded to `$HOME`. Paths are read verbatim — the entire file becomes
the prompt, so do **not** put comments inside a prompt file (they would be sent
to the model). Keep guidance in this README or in `daimonos.toml` comments.

## Getting the baseline defaults (no source needed)

Because the defaults are embedded in the binary, you can recover them at runtime
— you don't need this repo:

```bash
daimonos --print-prompt <name>       # print one default to stdout (name is one
                                     #   of: agent_system, mcp_instructions,
                                     #   kgl_hint, summary)
daimonos --dump-prompts              # scaffold all four into
                                     #   ~/.config/daimonos/prompts/
daimonos --dump-prompts /path/dir    # ...into a custom directory
daimonos --dump-prompts --force      # overwrite existing files
```

`--dump-prompts` skips files that already exist (unless `--force`) and prints a
ready-to-paste `[prompts]` block pointing at the scaffolded files, so you start
from the baseline and can diff your edits against it.

## Additional user instructions for agent runtimes

`daimonos agent`, `daimonos chat`, and ACP optionally append user-specific
instructions to the resolved `agent_system` prompt. Put them at:

```text
~/.config/daimonos/agent-instructions.md
```

`$XDG_CONFIG_HOME` replaces `~/.config` when set. A missing default file means
"no additional instructions" and is silently ignored. To use another file:

```bash
daimonos agent "task" --agent-instructions /path/to/rules.md
daimonos chat --agent-instructions /path/to/rules.md
daimonos acp --agent-instructions /path/to/rules.md
```

The file is appended verbatim with only a blank-line separator. An explicitly
selected unreadable file is an error; an existing but unreadable default file is
also an error so configured rules are never silently omitted. This mechanism
does not affect `daimonos --mcp`, whose host-facing prompt is
`mcp_instructions`.

## The prompts

| File | Used by | Purpose |
|------|---------|---------|
| `agent_system.md` | `daimonos agent`, `daimonos chat`, ACP | Core agent system prompt. Steers tool-use strategy (execute_script preference). |
| `mcp_instructions.md` | `daimonos --mcp` | Server `instructions` sent to the MCP host. Includes the **terse-output** directive that materially affects output token cost. |
| `kgl_hint.md` | `daimonos --mcp` (only when KGL auto-index is on) | Nudge to orient via the knowledge graph before reading source. |
| `summary.md` | context compaction (all interactive runtimes) | System prompt for the one-shot summarizer that replaces evicted turns. |

## WARNING

Changing these changes how the agent behaves. In particular:

- Removing the `execute_script` guidance from `agent_system.md` /
  `mcp_instructions.md` typically **increases** round-trips and token cost.
- Removing the terse directive from `mcp_instructions.md` **increases** output
  tokens.
- A vague `summary.md` degrades what survives compaction on long sessions.

Edit deliberately, and prefer overriding via `[prompts]` (leaving these
committed defaults intact) so you can compare against the baseline.

## `summary` override precedence

`summary` has an extra runtime override from the agent-env file
(`DAIMONOS_AGENT_SUMMARY_PROMPT`), which lives in the same config domain as the
other compaction knobs. Precedence is:

```
DAIMONOS_AGENT_SUMMARY_PROMPT  >  [prompts].summary  >  embedded summary.md
```
