# ADR-006: Privacy-first LLM observability over OpenTelemetry

**Date:** 2026-07-21
**Status:** Accepted
**Tracks:** Vikunja project 186, tasks #1-#7
**Relates to:** ADR-001 (provider boundary), ADR-002 (compaction),
ADR-003 (ACP-MCP bridge)

## Context

Daimonos currently exposes three different operational views:

- secure structured process logs describe lifecycle events and bounded resource
  telemetry;
- SQLite analytics aggregate tool calls, token savings, and completed one-shot
  agent runs;
- the optional token debug log records per-generation usage and compaction
  events.

These views are deliberately local and useful, but they do not reconstruct the
causal path of one agent turn. A prompt can make several LLM requests, invoke
native and forwarded MCP tools, compact history, retry after context overflow,
and eventually terminate or fail. Aggregates cannot answer which generation or
tool caused latency, cost, retry, or failure.

Langfuse can visualize this hierarchy and compare models or configurations. It
accepts standard OpenTelemetry traces at `/api/public/otel`, including traces
from Rust. Daimonos must not make its execution model dependent on Langfuse,
must remain useful offline, and must not export workspace or credential content
under default settings.

## Decision

### D1 — OpenTelemetry is the integration boundary

Daimonos emits OpenTelemetry spans through a backend-neutral exporter.
Langfuse is the first documented OTLP destination, not a core runtime
dependency and not an LLM proxy.

The runtime does not call Langfuse application APIs. Langfuse-specific
attributes are limited to the documented OTLP semantic layer needed to classify
traces and observations. Other OTLP collectors may consume the same spans.

### D2 — Observability is optional and fail-open

OTLP export is disabled by default. When enabled:

- spans enter a bounded in-process queue;
- a background batch exporter performs network I/O;
- a full queue drops telemetry rather than blocking an agent turn;
- exporter errors are reported only through secure local logging;
- shutdown attempts a bounded flush, then allows process exit;
- initialization or export failure never fails an ACP, chat, agent, MCP, or
  daemon operation.

No provider, tool, compaction, or session code may await an OTLP network
request on its hot path.

### D3 — One trace represents one prompt

For interactive frontends, each prompt is a root trace. For one-shot
`daimonos agent`, the run is one root trace. A long-lived ACP or chat session
does not become one long-lived trace because it may span hours or process
restarts.

Related prompt traces carry the same `langfuse.session.id`:

- ACP: ACP `SessionId`;
- chat: persisted chat session ID;
- one-shot agent: external session ID when supplied, otherwise a generated
  process-local run ID.

Retries remain children of the original prompt trace. Concurrent sessions have
independent active trace contexts.

### D4 — Span hierarchy

The canonical hierarchy is:

```text
agent.prompt
├── llm.generation
├── tool.call
│   └── tool.call              # execute_script child operations, when visible
├── mcp.remote_tool
├── context.compaction
│   └── llm.generation         # summary request
└── agent.retry
    └── llm.generation
```

MCP bridge connection/build/shutdown spans belong to session lifecycle traces
when no prompt is active. They must not be falsely attached to an unrelated
prompt merely because tasks share a Tokio worker.

### D5 — Stable metadata vocabulary

Attribute names are low-cardinality unless explicitly identified otherwise.
IDs are correlation values, not metric dimensions.

#### Prompt/root attributes

| Attribute | Meaning |
|---|---|
| `langfuse.trace.name` | `agent.prompt` or `agent.run` |
| `langfuse.session.id` | ACP/chat/external session correlation |
| `daimonos.runtime.mode` | `acp`, `chat`, or `agent` |
| `daimonos.turn.index` | zero-based user turn index |
| `daimonos.workspace.id` | one-way hash of canonical workspace path |
| `gen_ai.request.model` | selected model |
| `daimonos.context.window` | resolved model context window |
| `daimonos.context.used` | final generation's measured occupancy |
| `daimonos.tools.exposed` | number of schemas sent to the provider |
| `daimonos.prompt.call_count` | LLM generations in the turn |
| `daimonos.prompt.tool_count` | tool calls in the turn |
| `daimonos.stop_reason` | normalized terminal reason |
| `error.type` | bounded error class, when failed |

#### Generation attributes

| Attribute | Meaning |
|---|---|
| `langfuse.observation.type` | `generation` |
| `langfuse.observation.model.name` | effective model |
| `gen_ai.request.max_tokens` | requested output limit |
| `gen_ai.request.temperature` | sampling temperature, when set |
| `daimonos.thinking.level` | normalized thinking level |
| `daimonos.generation.kind` | `agent` or `compaction_summary` |
| `daimonos.generation.ordinal` | call index within prompt |
| `daimonos.context.prompt_tokens` | measured prompt occupancy |
| `gen_ai.usage.input_tokens` | canonical non-cached input tokens |
| `gen_ai.usage.output_tokens` | output tokens |
| `daimonos.usage.cache_read` | cache-read tokens |
| `daimonos.usage.cache_write` | cache-write tokens |
| `langfuse.observation.usage_details` | equivalent usage map |
| `langfuse.observation.cost_details` | provider-normalized USD cost map |
| `daimonos.time_to_first_token_ms` | first streamed content latency |
| `gen_ai.response.finish_reasons` | normalized stop reason |
| `daimonos.context_overflow` | classified context overflow |

#### Tool attributes

| Attribute | Meaning |
|---|---|
| `daimonos.tool.name` | exposed tool name |
| `daimonos.tool.kind` | native, plugin, script, or remote MCP |
| `daimonos.mcp.server` | forwarded server alias, when remote |
| `daimonos.tool.status` | success, error, blocked, timeout, or unavailable |
| `daimonos.tool.request_tokens_est` | request-size estimate |
| `daimonos.tool.response_tokens_est` | returned-size estimate |
| `daimonos.tool.saved_tokens_est` | estimated structural savings |
| `daimonos.tool.redirect` | plugin redirect hit |
| `daimonos.tool.filtered` | semantic output filter hit |
| `daimonos.tool.read_dedup` | unchanged-read suppression hit |
| `daimonos.tool.batch_size` | child operation count |

#### Compaction and recovery attributes

| Attribute | Meaning |
|---|---|
| `daimonos.compaction.trigger` | proactive or reactive overflow |
| `daimonos.compaction.strategy` | normalized strategy |
| `daimonos.compaction.high_water` | configured trigger ratio |
| `daimonos.compaction.low_water` | configured target ratio |
| `daimonos.compaction.tokens_before_est` | pre-compaction estimate |
| `daimonos.compaction.tokens_after_est` | post-compaction estimate |
| `daimonos.compaction.evicted_turns` | evicted turn count |
| `daimonos.compaction.evicted_messages` | evicted message count |
| `daimonos.compaction.summary_model` | summary model |
| `daimonos.compaction.fallback_drop` | summary failure fallback |
| `daimonos.retry.reason` | overflow, explicit retry, or transport recovery |
| `daimonos.truncate.turn_index` | first removed user turn |
| `daimonos.cancel.reason` | client, transport, timeout, or policy |

### D6 — Metadata-only is the default privacy boundary

Default spans must not contain:

- system or user prompts;
- assistant text or thinking;
- image bytes or image URIs;
- source code, file contents, diffs, or canonical workspace paths;
- tool argument or result bodies;
- command lines beyond an approved bounded category;
- environment variable values;
- HTTP headers, authorization values, API keys, or MCP credentials;
- full provider error bodies, which may echo requests or credentials.

Workspace correlation uses a one-way hash. Errors use bounded classes and
status codes; full errors remain in secure local logs.

Content capture, if implemented, is a separate explicit opt-in. It applies
length caps and redaction before span creation, not in the exporter after
sensitive values have already entered telemetry memory. Thinking and credential
material remain excluded even when ordinary prompt/output capture is enabled.

OpenTelemetry baggage must never carry sensitive values because baggage can
propagate to downstream services.

### D7 — Existing local telemetry remains authoritative

Langfuse/OTLP does not replace:

- SQLite tool analytics and token-savings reports;
- secure rotating process logs;
- process resource telemetry;
- token benchmark artifacts.

OTLP is the causal distributed-trace view. SQLite remains the durable local
aggregate view. Structured logs remain the process/debug view. Implementations
may share normalized value objects, but an exporter outage cannot reduce local
coverage.

### D8 — Cardinality and retention are bounded

High-cardinality IDs may appear on spans for correlation but are not promoted
to metric labels. Raw model IDs and tool names are accepted because their sets
are bounded by configuration. Paths, arbitrary error strings, prompt hashes,
and tool payload hashes are not emitted as labels.

Sampling is parent-based. A sampled prompt includes its descendants so a trace
never contains disconnected tool or generation observations.

Langfuse retention is an operator choice. Daimonos documents it but does not
assume it matches local SQLite retention.

### D9 — Instrument the shared core, not each frontend independently

Root trace creation belongs at the agent/chat/ACP frontend boundary where the
session and turn identity are known. Child instrumentation belongs in shared
code:

- LLM generations around calls in `agent::run` and compaction summarization;
- native tools around `tool_facade` invocation;
- forwarded tools and bridge lifecycle in `mcp_bridge`;
- compaction decisions in `AgentSession`;
- retry, truncation, and cancellation where their authoritative state changes.

Provider adapters retain provider-specific parsing and error classification per
ADR-001. They do not acquire Langfuse dependencies.

### D10 — Verification gates

Implementation is incomplete until it has:

1. unit tests against an in-memory or mock span exporter;
2. mock OTLP integration tests with no external credentials;
3. a secret-bearing fixture proving metadata-only spans omit sensitive values;
4. streaming tests that verify time-to-first-token and terminal usage;
5. cancellation and bounded-flush tests;
6. disabled-versus-enabled overhead measurements;
7. a self-hosted Langfuse smoke test documented as optional.

The target overhead budget will be set from measured baselines. Export is not
enabled by default until queue behavior, shutdown, and privacy gates pass.

## Configuration direction

The implementation will add an `[observability]` table. Exact field names are
finalized with the exporter task, but the contract requires:

- `enabled = false`;
- OTLP HTTP endpoint;
- public/secret key environment-variable names, never literal secret values;
- environment and release labels;
- parent-based sample ratio;
- bounded queue, batch delay, and flush timeout;
- explicit content-capture controls defaulting off.

## Consequences

### Positive

- One prompt can be diagnosed across generations, tools, retries, and
  compaction.
- Model, configuration, and token-saving strategies become directly
  comparable.
- Langfuse can be replaced by another OTLP backend without changing agent
  semantics.
- Local/offline behavior and existing analytics continue unchanged.

### Negative

- OpenTelemetry adds dependencies, binary size, background tasks, and shutdown
  behavior.
- Correct async context propagation requires care across spawned tasks.
- Telemetry creates a new data-egress surface that demands security testing.
- High-volume traces require sampling and retention management.

## References

- Langfuse SDK and OpenTelemetry overview:
  <https://langfuse.com/docs/observability/sdk/overview>
- Langfuse native OpenTelemetry ingestion:
  <https://langfuse.com/integrations/native/opentelemetry>
- OpenTelemetry Rust:
  <https://opentelemetry.io/docs/languages/rust/>
