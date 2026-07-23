# ADR-008: Programmatic LLM sub-calls in `execute_script`

**Date:** 2026-07-22
**Status:** Proposed
**Tracks:** Vikunja #1049 (project 183); follows the RLM research spike #1044
**Relates to:** ADR-001 (provider boundary), ADR-006 (observability),
ADR-007 (context-offload handles), ADR-002 (compaction).
**Depends on:** the "`execute_script` reachable in the internal agent loop"
prerequisite (shared with #1045 and ADR-007's primary path).

## Context

The RLM research (#1044) identifies *programmatic sub-agent calling* as the
highest-leverage mechanism for keeping the root model locally in-distribution
(LID): tools and sub-agents are functions in a code REPL, and their outputs live
in REPL variables that the root model never sees. In the paper this is the
`llm_query_batched(prompts) -> outputs` primitive that drove ~10× eval lift and
8–32× length generalization — the root model fans out classification /
extraction / summarization over chunks and only reads back a compact aggregate.

Daimonos's `execute_script` (Starlark) already keeps intermediate *data* out of
the root context. What it lacks is the ability to run **bounded LLM sub-calls
from inside a script**, so the intermediate *model outputs* also stay in the
sandbox. This ADR designs that primitive.

It is deliberately design-only: the mechanism has real prerequisites and a
safety/cost surface that must be settled before code.

## Prerequisite (must land first)

`execute_script` is **not reachable from the internal agent loop** today: it has
`to_request: None` and is served only on the MCP-server path; the Starlark
sandbox holds no `LlmProvider`. A sub-call primitive is meaningless until:

1. `execute_script` is dispatchable inside `agent::run` under the prompt root,
   and
2. the sandbox is given a provider/session handle to issue sub-calls through.

This is the same prerequisite that blocks #1045 (execute_script child-op spans)
and ADR-007's in-sandbox consumption path. **Recommendation:** scope it as its
own feature ticket ("`execute_script` in the internal agent loop"); it unblocks
all three at once. This ADR assumes it exists.

## Decision

### D1 — Sandbox primitives

Two Starlark builtins, available only when enabled and only when a provider
handle is present:

```
llm_query(prompt: str, *, model: str = <session model>,
          max_tokens: int = <cfg>, temperature: float = 0.0) -> str
llm_query_batched(prompts: list[str], **opts) -> list[str]
```

Outputs are plain strings that land in sandbox variables. As with all
`execute_script` data, they never enter the conversation unless the script
copies them into `result` — which the offload guidance (#1047) tells the model
not to do wholesale.

### D2 — Provider boundary (ADR-001)

Sub-calls go through the existing `LlmProvider` the session already holds — not
a new client, key, or endpoint. Provider-specific parsing/error-classification
stays in the adapter (ADR-001). A failed sub-call surfaces as a typed Starlark
error the script can catch; it never silently returns wrong data.

### D3 — Bounded controls (all required)

- **Token/cost budget** per script (and optionally per call); exceeding it makes
  further sub-calls raise. Sub-call usage counts against the turn's budget.
- **Fan-out cap** — `llm_query_batched` accepts at most `max_batch` prompts;
  excess is a Starlark error, never silent truncation.
- **Concurrency** — a batched call runs prompts concurrently up to a bounded
  worker count, then returns results aligned to input order.
- **Recursion depth** — MVP caps depth at 1: a sub-call is a leaf model call and
  cannot itself spawn scripts/sub-calls. (The paper's recursive RLM is a future
  extension, explicitly out of scope here to bound blast radius.)
- **Timeout + cancellation** — each sub-call is bounded by a timeout and honors
  the sandbox's existing cancel flag (client cancel / turn abort), so a script
  fanning out N calls cancels promptly.

### D4 — Cost and usage accounting

Sub-call usage (input/output/cache tokens, cost) rolls into the turn's
cumulative `Usage`, exactly as compaction-summary calls do today, and into the
SQLite analytics. The root turn's reported cost includes its sub-calls.

### D5 — Observability (produces the #1045 tree)

Each sub-call emits an `llm.generation` span (e.g. `daimonos.generation.kind =
script_subcall`) **nested under the script's `tool.call` (kind = script)** span
— which is precisely the `tool.call → llm.generation` nesting deferred in #1045.
Building this feature is what makes that tree real. A batched call emits one
generation span per prompt (accurate per-call usage/TTFT), grouped under the
script tool.call, plus a batch-size attribute on the parent.

### D6 — Privacy (ADR-006 D6)

Sub-call **prompts and outputs are conversation-adjacent content and are never
exported to telemetry** — only usage/cost/latency metadata on the generation
spans, identical to the main-loop generations. Prompts are built by the script
from workspace data; the same content boundary applies.

### D7 — Opt-in / configuration

Off by default. A `[tools.script_llm]` table (names finalized with
implementation) gates it: `enabled`, `max_subcalls_per_script`, `max_batch`,
`per_script_token_budget`, `subcall_timeout_ms`, and a default sub-call model
(defaults to the session model).

### D8 — Verification gates

1. single + batched sub-call round-trip (outputs aligned, stay in sandbox);
2. per-script and per-call budget enforcement (further calls raise);
3. fan-out cap → error, not truncation;
4. timeout + cancellation (in-flight fan-out cancels promptly);
5. usage/cost roll-up into the turn;
6. observability: `script_subcall` generations nest under the script `tool.call`
   (closing #1045), with no prompt/output content on spans (extends the ADR-006
   secret corpus);
7. disabled-by-default: no sandbox primitive present unless enabled + provider.

## Consequences

### Positive
- Brings the RLM mechanism most responsible for length/domain generalization to
  Daimonos: fan-out with intermediate model outputs held out of the root
  context (LID).
- Delivers the #1045 span tree as a side effect; reuses ADR-001/006 machinery
  rather than adding new surfaces.
- Composes with ADR-007 handles (fan out over a handle's chunks in-sandbox) and
  the #1047 offload guidance.

### Negative
- Real cost/latency: N sub-calls per script; the budget/fan-out caps bound it,
  but a fan-out is inherently more expensive than one call. (Overhead
  quantification, angle 4 of #1044, is measured here using these spans.)
- Gated on the execute-script-in-agent-loop prerequisite — non-trivial runtime
  work before any of this lands.
- New safety surface (cost blow-up, injected-content fan-out); mitigated by
  caps + provider limits + default-off, not eliminated.
- The model must learn a more advanced pattern than #1047's; may need further
  prompt guidance.

## Follow-up implementation issues (create on acceptance, in order)
1. **`execute_script` reachable in the internal agent loop** with a provider/
   session handle (the prerequisite; also unblocks #1045 and ADR-007 path 2).
2. `llm_query` / `llm_query_batched` builtins + provider wiring (ADR-001).
3. Bounded controls: budget, fan-out cap, concurrency, depth, timeout/cancel.
4. Usage/cost roll-up + analytics.
5. `script_subcall` observability nesting (closes #1045) + privacy-corpus test.

## References
- RLM research findings: `docs/research/rlm-harness-evaluation.md`
- ADR-001 (provider boundary), ADR-006 (observability/privacy), ADR-007
  (context-offload handles)
- RLM reference implementation: <https://github.com/alexzhang13/rlm>
