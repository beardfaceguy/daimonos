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
| `daimonos session-daemon` | Persistent daemon-owned agent sessions over the local Unix socket | new |
| `daimonos session import/export` | Transfer versioned portable session archives | new |
| `daimonos mcp` | MCP server over stdio | `daimonos --mcp` |
| `daimonos mcp --socket <path>` | MCP server over a Unix socket | `daimonos --mcp-socket <path>` |
| `daimonos daemon` | Compact opcode protocol over a Unix socket | bare `daimonos` |

Global options such as `--workspace` and `--config` precede the subcommand:

```bash
daimonos --workspace /path/to/project mcp
daimonos --workspace /path/to/project --socket /tmp/daimonos.sock daemon
daimonos --workspace /path/to/project session-daemon
daimonos --workspace /path/to/project session import session.json
```

The legacy forms remain supported so existing editor configurations and service
units do not require migration. New configuration should use the explicit
subcommands because process listings, help output, and diagnostic logs then
describe the selected runtime unambiguously.

`agent --interactive` requires both terminal input and output. `--print`
explicitly forces one-shot output when flags are composed; without
`--interactive`, one-shot mode remains the default.

## One-shot sessions and the JSON result

Three opt-in options change a one-shot `agent` run (ADR-036). A run without
them behaves as before.

```bash
daimonos agent --session-id run-42 --output json "Change greet() to return 'hi'."
daimonos agent --resume run-42 --output json "The tests failed: <output>"
```

| Option | Effect |
|---|---|
| `--session-id <ID>` | Start a named session and save it when the run ends. An existing id is refused. |
| `--resume <ID>` | Continue the session. The task is the new input. |
| `--output json` | Print one JSON result document on stdout. |

The three options conflict with `--interactive` and `--dry-run`. `--session-id`
and `--resume` conflict with each other.

Combinations with the reviewer-runtime options:

- `--session-id` and `--resume` are refused with `--final-output`,
  `--model-profile`, and `--worker-config`. These combinations are not built.
  The session record does not hold the endpoint of a model profile.
- `--output json` is refused with `--final-output` and with
  `--capability-probe-payload` (a probe requires `--final-output`). Both
  `--output json` and `--final-output` own stdout. ADR-036 Decision 14 defines
  the combination. It is not built.
- `--output json` works with `--model-profile`. The field `provider` is then
  the `provider_label` of the profile.
- `--output json` works with `--audit-path`. The run writes the `run_end`
  audit event before the document reaches stdout. A failed session save still
  prints the document, exits with code 1, and is recorded as `error`.
- `--output json` works with `--worker-config`. A worker that is lost during
  the run gives a document with `stop_reason: "error"` and exit code 22. A
  worker run that does not start (a worker cannot start, or an allowlist entry
  names no worker tool) prints no document and exits with code 22 or 2.
- A session works with `--tool-allowlist`. The allowlist changes the offered
  tools, so a resume needs the same allowlist.
- `--workspace-read-only` does not change the offered tools. It belongs to the
  run, not to the session: a resume can use another setting than the first run.
- A run that fails before the agent loop (for example, the thought log cannot
  be opened) saves nothing. The id of a new session stays free, and a resumed
  session is unchanged. The top-level `--session-id` (before
`agent`) is a different option: it filters `--stats`.

**Sessions**

- An id has 1 to 128 characters from `A-Z`, `a-z`, `0-9`, `-`, and `_`.
- The store is `~/.daimonos/oneshot-sessions`, or `sessions/oneshot` under
  `--state-dir`. `chat --list` and `chat --resume` do not see these sessions.
- A resume uses the saved system prompt, model, and thinking level. It sends the
  saved messages and the new input. The system prompt is not rebuilt, so a
  change of the workspace instructions or skills does not reach a resumed
  session.
- A resume is refused when `--model` differs from the saved model, when the
  selected provider or its endpoint differs from the saved one, or when the
  tool list differs in order, name, description, or input schema. Start a new
  session to change one of them. The endpoint is the base URL of the provider
  and of each secondary provider. The record holds a SHA-256 digest of it, not
  the URL.
- With outbound MCP servers, the tool list depends on which servers connect. A
  server that is not connected at resume time changes the list, and the resume
  is refused with `session_tools_changed`.
- One process owns a session at a time. A second run on the same id is refused
  while the first run is alive.
- The run saves the session for each stop reason, also after a provider error.
  A failed first run therefore consumes its id: continue it with `--resume`.
- The saved history is one that a provider accepts with a new input after it.
  A trailing assistant message without text, tool call, or image is not saved
  (it is empty, or it holds only thinking). Each saved tool call has a result:
  an open tool call gets an error result. Truncated text after `max_tokens` is
  saved. After a failed first call the history ends with the task, so the next
  resume sends two user messages in sequence.
- Old tool results are pruned inside a run, as in a run without a session. The
  loop prunes before it builds a request, and the saved history is the pruned
  history. A prune changes earlier messages, so the requests after it can miss
  the provider cache up to the changed message.
- A session run has no compaction. A context overflow ends the run with
  `context_overflow: true`. Start a new session then.
- A resumed run without `--output json` prints only the assistant text of that
  invocation.
- With `DAIMONOS_AGENT_PROMPT_CACHE=on`, a session run marks the whole message
  list as the stable prefix, so the provider adapter can set its cache
  breakpoint at the end of the conversation. With the default `off`, a session
  run sends no cache marker. For the direct Anthropic provider this is checked
  by request-builder tests only. A real run with a cache read exists for
  OpenRouter only.
- Each run writes one `agent_runs` row. The column `oneshot_session_id` holds
  the session id, or NULL.

**Exit codes**

| Case | Exit code | Output |
|---|---|---|
| The run ended with `end_turn`, `max_tokens`, `refusal`, or `aborted` | 0 | The document, or the assistant text |
| The run ended with `error` (a provider error or a context overflow) | 1 | The document, when `--output json` is set |
| The session was not saved | 1 | The document with `session_saved: false`, and a message on stderr |
| The run was refused before it started | 2 | Nothing on stdout. One JSON line on stderr. |

The exit code does not show if the work is complete: a truncated run
(`max_tokens`) and a model refusal exit with code 0. Read `stop_reason` in the
document. These exit codes are the same with and without the new options.

A refusal has this form on stderr:

```json
{"error":{"code":"run_configuration_invalid","reason":"session_exists","message":"..."}}
```

The reasons are `session_id_invalid`, `session_exists`, `session_not_found`,
`session_owned_elsewhere`, `session_changed`, `session_not_resumable`,
`session_model_mismatch`, `session_provider_mismatch`,
`session_endpoint_mismatch`, `session_tools_changed`, and `state_unusable`.

`session_owned_elsewhere` can also occur for a moment after a run ended, while
a tool child process of that run starts. Try the resume again.

**JSON result document**

`--output json` prints exactly one JSON document and one newline on stdout for
each run whose agent loop returned a result. This includes a provider error and
a context overflow, so a caller can record the tokens of a failed run. A run
that fails before the loop starts prints no document. The run writes the
document in one write after it is complete, so stdout never holds a part of a
document. A failure of the thought log or of the session save is reported after
the document.

| Field | Type | Content |
|---|---|---|
| `schema_version` | integer | `1`. It changes when a field is removed or changes its meaning. |
| `session_id` | string or null | The id of `--session-id` or `--resume` |
| `resumed` | boolean | True for `--resume` |
| `session_saved` | boolean or null | Null without a session option |
| `session_error` | string or null | The reason of a failed save |
| `provider`, `model` | string | The provider and the model of the run |
| `stop_reason` | string | `end_turn`, `tool_use`, `max_tokens`, `refusal`, `error`, or `aborted` |
| `error_message` | string or null | The provider error |
| `context_overflow` | boolean | True when the request did not fit the context window |
| `final_text` | string | The text of the last assistant message of this invocation |
| `turns` | integer | The messages that this invocation added after its input |
| `usage` | object | `input_tokens`, `output_tokens`, `reasoning_output_tokens` (or null), `thinking_bytes`, `cache_read_tokens`, `cache_write_tokens`, `cost_usd` |
| `evidence` | object | `status`: `unknown`, `no_mutations`, `verified`, or `unverified` |
| `duration_ms` | integer | Time from the start of the run function to the document. It includes the system prompt, the MCP server start, the agent loop, and the session save. It excludes the process start, the configuration, the provider setup, and the claim of the session. |

`usage`, `stop_reason`, and `turns` cover this invocation only, and they have
the same values as the `agent_runs` row of the run. `input_tokens` does not
include the cached tokens.

Interactive controls include Up/Down prompt history, PageUp/PageDown transcript
scrolling, Home/End jumps, and `/help`. Pass `--no-color` for monochrome
rendering. History and scrollback limits are configured under `[tui]`.

`session-daemon` derives its default socket from the canonical workspace, while
`session.socket_path` or `--socket` can override it. The socket is created mode
`0600` in an owner-controlled directory, verifies local peer credentials, and
is removed during orderly Ctrl-C/SIGTERM shutdown.
ACP and daemon sessions use separate SQLite stores until ACP is routed through
the daemon. Transactional generations reject stale whole-history writers. Explicit
`stop_session` deletes the daemon-owned saved conversation; daemon shutdown
preserves it for restart.
The compatibility `chat` store assumes one active editor process per session
id; daemon-owned sessions remain the concurrency-safe path.

Remote Android access is explicit opt-in:

```bash
daimonos --workspace /path/to/project session-daemon \
  --remote-listen 127.0.0.1:9470 \
  --remote-origin https://remote.example
```

The gateway is loopback-only and must be published through a TLS reverse proxy
to provide WSS. It prints one single-use five-minute pairing claim. After the
phone submits its Ed25519 public key, the daemon prints the device fingerprint
and requested capabilities; type
`approve <pairing-id> <capability>...` with the exact subset to grant, or
`deny <pairing-id>` on the daemon's local stdin. Claims renew before expiry and
after each submission. Type `revoke <device-id>` to invalidate every
in-memory ticket and disconnect that device. Remote `approve_always` is removed
unless `--remote-allow-always` is explicitly set. Browser Origins require an
exact `--remote-origin`; native Android clients omit Origin.

When the loopback reverse proxy overwrites `X-Forwarded-For`, pass
`--remote-trust-proxy-headers` so admission limits are keyed by the originating
client rather than the proxy. The flag is rejected for non-loopback peers and
must not be used with a proxy that appends to an untrusted client-supplied
header.

Inspection operations (`--stats`, `--print-config-path`, `--print-prompt`, and
`--dump-prompts`) remain top-level flags because they do not launch a persistent
runtime.
