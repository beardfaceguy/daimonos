# Runtime Modes

Daimonos remains one distributable binary. Its runtime modes share protocol,
configuration, analytics, and tool implementations, while their launch paths
are separated so agent runtimes do not initialize tool-server watchers or
listeners.

| Recommended invocation | Purpose | Compatibility form |
|---|---|---|
| `daimonos acp` | ACP agent for Zed and other ACP clients | unchanged |
| `daimonos agent "<task>"` | One-shot autonomous agent | unchanged |
| `daimonos agent --interactive ["<task>"]` | Opt-in full-screen terminal agent | non-TTY falls back to one-shot mode |
| `daimonos chat` | Interactive agent REPL | unchanged |
| `daimonos mcp` | MCP server over stdio | `daimonos --mcp` |
| `daimonos mcp --socket <path>` | MCP server over a Unix socket | `daimonos --mcp-socket <path>` |
| `daimonos daemon` | Compact opcode protocol over a Unix socket | bare `daimonos` |

Global options such as `--workspace` and `--config` precede the subcommand:

```bash
daimonos --workspace /path/to/project mcp
daimonos --workspace /path/to/project --socket /tmp/daimonos.sock daemon
```

The legacy forms remain supported so existing editor configurations and service
units do not require migration. New configuration should use the explicit
subcommands because process listings, help output, and diagnostic logs then
describe the selected runtime unambiguously.

`agent --interactive` requires both terminal input and output. `--print`
explicitly forces one-shot output when flags are composed; without
`--interactive`, one-shot mode remains the default.

Inspection operations (`--stats`, `--print-config-path`, `--print-prompt`, and
`--dump-prompts`) remain top-level flags because they do not launch a persistent
runtime.
