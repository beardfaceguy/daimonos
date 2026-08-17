# LLM Observability (OpenTelemetry / Langfuse)

Optional, privacy-first distributed tracing for Daimonos agent runtimes
(ADR-006). It makes one agent prompt inspectable as a causal trace — prompt →
LLM generations → native/remote tools → compaction/retry → outcome — without
coupling execution to any backend and without exporting workspace or credential
content.

- **Disabled by default.** Nothing is exported until you set
  `[observability].enabled = true`.
- **Fail-open.** A full queue drops spans rather than blocking a turn;
  initialization, export, and shutdown failures never fail an agent turn.
- **Local telemetry stays authoritative.** SQLite analytics and secure process
  logs are unchanged; OTLP is an additional, causal view.
- **Metadata-only.** Prompts, outputs, thinking, source, file contents, tool
  arguments/results, commands, environment values, headers, and credentials are
  never exported under defaults.

For the full field-by-field config reference see the `[observability]` table in
[configuration.md](configuration.md#observability--otlplangfuse-tracing). This
document is the operational runbook.

Observability is only active in the runtimes that own a prompt/turn identity:
`daimonos agent`, `daimonos chat`, and the ACP agent. In pure MCP-server mode
(serving an external client such as Cursor) there is no Daimonos prompt root, so
export is not engaged.

---

## Trace model

One trace per prompt. Related prompts in the same ACP/chat/one-shot session
share `langfuse.session.id`. Canonical hierarchy:

```text
agent.prompt
├── llm.generation
├── tool.call                 # native/opcode tools (+ exec plugin-redirect)
├── mcp.remote_tool           # forwarded MCP tools (server alias, timeout)
├── context.compaction
│   └── llm.generation        # summary request (kind = compaction_summary)
└── agent.retry               # context-overflow / explicit retry
    └── llm.generation
agent.truncate                # user-initiated history truncation (own root when no prompt)
mcp.bridge                    # forwarded-server build/shutdown lifecycle (own root)
```

Attribute vocabulary is defined in ADR-006 §D5. Everything is low-cardinality
except correlation IDs (`langfuse.session.id`, `daimonos.workspace.id`), which
are never promoted to metric labels. Two lifecycle attributes not in the
original D5 table were added for MCP correlation: `daimonos.mcp.event`
(`build`/`shutdown`) and `daimonos.mcp.servers` (connected count).

---

## Setup

### Langfuse Cloud

1. Create a project; copy its **public** and **secret** API keys.
2. Export the keys under the names Daimonos reads (defaults shown):
   ```bash
   export LANGFUSE_PUBLIC_KEY=pk-lf-...
   export LANGFUSE_SECRET_KEY=sk-lf-...
   ```
3. Set the region-specific OTLP traces endpoint and enable export:
   ```toml
   [observability]
   enabled = true
   # US:  https://us.cloud.langfuse.com/api/public/otel/v1/traces
   # EU:  https://cloud.langfuse.com/api/public/otel/v1/traces
   endpoint = "https://us.cloud.langfuse.com/api/public/otel/v1/traces"
   basic_auth = true
   environment = "production"
   ```

The endpoint must be the **signal-specific** traces path ending in
`/v1/traces` — Daimonos configures the traces exporter directly, so the generic
`/api/public/otel` base is not sufficient.

### Self-hosted Langfuse

```bash
git clone https://github.com/langfuse/langfuse && cd langfuse
docker compose up -d          # serves the UI + OTLP ingest on :3000
```

Create a project in the local UI, copy its keys, then:

```toml
[observability]
enabled = true
endpoint = "http://localhost:3000/api/public/otel/v1/traces"
basic_auth = true
environment = "development"
```

### Unauthenticated OTLP collector

For a local collector without Basic Auth (e.g. an OpenTelemetry Collector):

```toml
[observability]
enabled = true
endpoint = "http://localhost:4318/v1/traces"
basic_auth = false
```

---

## Smoke test

An ignored integration test emits one full prompt trace to a real endpoint so
you can verify rendering end-to-end. It never runs in CI (no credentials).

```bash
export LANGFUSE_PUBLIC_KEY=pk-lf-... LANGFUSE_SECRET_KEY=sk-lf-...
export DAIMONOS_SMOKE_OTLP_ENDPOINT=http://localhost:3000/api/public/otel/v1/traces
cargo test --bin daimonos observability::tests::self_hosted_langfuse_smoke_test -- --ignored --nocapture
```

In the Langfuse UI, confirm one trace named `agent.prompt` (session
`smoke-session`) containing an `llm.generation` with usage/cost and
time-to-first-token, a `tool.call`, and a `context.compaction` with a nested
summary generation. Then drive a real `daimonos agent`/`chat`/ACP prompt and
confirm the same shapes appear with live data.

---

## Credential rotation

- Keys are read **only** from the environment variables named by
  `basic_auth_username_env` / `basic_auth_password_env`; literal secrets never
  appear in config files, config dumps, or initialization errors.
- To rotate: create the new key pair in the backend, update the environment
  values, and restart the Daimonos process (keys are read at startup). No config
  file change is needed if the variable names are unchanged.
- Rotation is zero-downtime for agent work: export failures are fail-open, so a
  brief window with a stale key drops spans but never blocks a turn.

## Sampling

`sample_ratio` is parent-based (`0.0`–`1.0`): a sampled prompt includes all its
descendants, so a trace is never partially exported. Start at `1.0` while
validating; lower it for high-volume production (e.g. `0.1`) to cut ingest cost.
Sampling is decided at the root, so per-prompt cost is all-or-nothing.

## Content capture (opt-in)

There is **no content capture today** — export is unconditionally metadata-only
and there is no config key to enable prompt/output capture. A future redacted
content mode will land only together with its length caps, redaction, and
privacy tests; thinking and credential material will remain excluded even then.
Until that ships, no configuration can cause Daimonos to export conversation
content.

For local debugging only, one-shot `daimonos agent` supports
`--debug-thoughts`. It writes streamed thinking text to
`~/.config/daimonos/thought-debug.log`, or to
`--debug-thoughts-path PATH` when explicitly supplied. Capture is off by
default, the file is truncated for each run and forced to mode `0600`, and its
content is never copied into token logs, SQLite analytics, or OpenTelemetry.
Treat the file as sensitive because model thinking can contain user or source
context.

## Retention

Retention is a **backend** concern, independent of Daimonos' local SQLite
retention (`[analytics].retention_days`). Configure trace retention in Langfuse
(project/data-retention settings) or your collector's pipeline. Daimonos does
not assume the backend's retention matches local analytics.

## Overhead & budgets

Measured against the disabled baseline:

| Dimension | Budget | Notes |
|-----------|--------|-------|
| Per-span latency (enabled) | < ~50 µs typical; hard CI ceiling ~1 ms/span | Span create + attribute record + close on the turn thread. Network I/O is off the hot path. |
| Turn latency added | Negligible vs. LLM round-trips | Export is batched on a background task; turns never await OTLP. |
| Memory | Bounded by `max_queue_size` × span size | A full queue drops spans (fail-open), so memory cannot grow unbounded. |
| CPU | Background batch exporter only | No per-turn CPU beyond cheap span construction. |
| Shutdown | Bounded by `flush_timeout_ms` | Shutdown attempts a bounded flush, then exits regardless. |

The `enabled_tracing_overhead_is_bounded` unit test guards against pathological
per-span regressions in CI; precise per-environment numbers come from the smoke
test / your own measurement. When enabled export is unreachable, turns proceed
at the disabled baseline (fail-open).

## Troubleshooting

- **No traces appear.** Confirm `enabled = true`, the env-var keys are set, and
  the endpoint ends in `/v1/traces`. Initialization/export failures are reported
  only through the local diagnostic log target `daimonos::observability_local`
  (never on the OTLP path) — check secure process logs.
- **`observability: initialization failed`.** Missing/invalid credentials or a
  malformed endpoint. Export is disabled; the runtime continues. The message
  names the offending env var but never prints secret values.
- **"ignored for runtime mode".** Export only engages for `agent`/`chat`/`acp`.
  MCP-server mode has no prompt root.
- **Spans dropped under load.** Expected fail-open behavior when the queue
  saturates. Raise `max_queue_size`/`max_batch_size` or lower `sample_ratio`.
- **Partial/disconnected observations.** Sampling is parent-based, so this
  should not happen; if it does, verify the frontend wraps the turn in the
  prompt span (`.instrument(prompt_span…)`).

## Disable / rollback

Set `enabled = false` (or remove the `[observability]` table) and restart — the
exporter and its background task are never constructed, and behavior returns to
the disabled baseline. No data migration or cleanup is required; local SQLite
analytics and process logs are unaffected. Because export is off by default,
rollback is simply reverting the one flag.

## Comparing models & token-saving strategies

Because usage, cost, latency, and time-to-first-token are attached per
`llm.generation`, and tool savings per `tool.call`
(`daimonos.tool.saved_tokens_est`, `redirect`/`filtered`/`read_dedup`), you can
compare configurations directly in the backend:

- Group by `gen_ai.request.model` / `langfuse.observation.model.name` to compare
  models on cost, output tokens, and TTFT for equivalent prompts.
- Use `daimonos.compaction.tokens_before_est` vs `tokens_after_est` and
  `evicted_turns` to evaluate compaction effectiveness.
- Filter on `daimonos.tool.redirect` / `filtered` / `read_dedup` and sum
  `saved_tokens_est` to quantify Daimonos' structural token savings.
- Segment by `deployment.environment.name` and `langfuse.release` to compare
  releases.
