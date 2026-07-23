# ADR-007: Context-offload handles for tool outputs

**Date:** 2026-07-22
**Status:** Proposed
**Tracks:** Vikunja #1048 (project 183); follows the RLM research spike #1044
**Relates to:** ADR-006 (LLM observability), ADR-002 (compaction);
depends on the "execute_script reachable in the internal agent loop"
prerequisite shared with #1045 / ADR-008.

## Context

Daimonos tools inline their full result into the model context. Token-
minimization (plugin redirect, exec output filtering, read dedup, compact
response fields) trims that inline, but a large payload — a whole-file read, a
long build log, a wide search — still enters the conversation verbatim. Over a
long session this is the "context rot" the RLM research (#1044) identifies:
each growing observation pushes the root model further out of distribution and
degrades quality.

`execute_script` already demonstrates the fix for *intermediate* data: values
computed in the Starlark sandbox stay there and only the compact `result` is
returned. But (a) even `result` is returned verbatim, and (b) a *direct* tool
call has no way to keep a large output out of context while still letting the
model act on it later.

This ADR proposes **context-offload handles**: a tool may return a compact,
referenceable handle instead of a large body. The model passes the handle to a
later operation that consumes it *without the bytes ever entering the
conversation* — the general form of what `execute_script` does for
intermediates, extended to all tool outputs. This is the RLM "context
offloading" mechanism (input held as a symbolic variable the root model never
directly sees).

Prompt guidance to prefer this pattern shipped in #1047; this ADR is the
mechanism that guidance can point at.

## Decision

### D1 — A handle is a compact reference plus bounded metadata

When a tool produces an output larger than a configured threshold and handles
are enabled, it may return a handle envelope instead of the raw body:

```json
{ "handle": "h:7",
  "kind": "file",            // file | exec | search | script
  "bytes": 48213,
  "lines": 902,
  "sha256": "…",
  "head": "first ~500 chars…" }
```

The `head` (and/or a one-line summary) is the only content inlined — enough for
the model to decide what to do. The full body lives server-side, bound to the
handle id. This preserves the ADR-006 D6 boundary: only size/hash/head metadata
is ever eligible for telemetry, never the full body.

### D2 — Handles live in a bounded, session-scoped store

Handles reference entries in an in-memory, per-session store:

- transient working data, **not** persisted (unlike ACP session history);
- bounded by a total-bytes cap with LRU eviction; a resolve of an evicted
  handle returns a typed "expired handle" error, never silently wrong data;
- cleared on session end.

Opaque ids (`h:<counter>` per session) keep cardinality bounded and carry no
path/content information.

### D3 — Two ways to consume a handle

1. **Slice it into context on demand** — a `read_handle(handle, offset, limit)`
   tool returns a bounded window of the value (the model explicitly chooses to
   inline a piece). This covers "show me the error near line 400".
2. **Operate on it in-sandbox** — an `execute_script` builtin `handle(id)`
   returns the bound value inside the Starlark sandbox, where the script
   filters/greps/summarizes it and returns a compact `result`. The bytes never
   enter the conversation. This is the primary, highest-value path and the
   direct analogue of the RLM REPL binding input to a `context` variable.

Path 2 requires `execute_script` to be reachable from the internal agent loop
with a session handle — the same prerequisite as #1045 / ADR-008. Path 1 works
without it.

### D4 — Opt-in and backwards-compatible

Handles are **off by default**; current inline behavior is unchanged. When
enabled, a per-tool opt-in set and a size threshold decide when a body becomes a
handle. Handles generalize the existing token-minimization ("trim the inline")
to a new tier ("don't inline at all"); the two compose — a redirect/filtered
result under the threshold still returns inline.

### D5 — Observability

Handle creation is an attribute on the producing `tool.call` span
(`daimonos.tool.handle = true`, plus the existing size/savings estimates).
`read_handle` and the `handle()` builtin resolve are `tool.call` children under
the prompt root (ADR-006 model). No body content on spans.

### D6 — Privacy

Handle stores hold workspace content in process memory only. Nothing about a
handle's body crosses the OTLP boundary; only `bytes`/`sha256`/kind metadata is
eligible, consistent with ADR-006 D6. Handles are never written to disk.

### D7 — Verification gates

1. create → resolve round-trip (full + bounded slice);
2. threshold + per-tool opt-in behavior; default-off preserves inline output;
3. eviction under the size cap → typed expired-handle error;
4. `execute_script` `handle()` builtin returns the bound value and keeps it out
   of the returned `result`;
5. observability: handle create/resolve spans carry only metadata;
6. no handle body appears in any exported span (extends the ADR-006 secret
   corpus).

## Configuration direction

A new `[tools.handles]` table (exact names finalized with implementation):

- `enabled = false`;
- `min_bytes` threshold above which an opted-in tool returns a handle;
- `max_store_bytes` total per-session cap (LRU eviction);
- per-tool opt-in list (initially `read_file`, `exec`, `search`).

## Consequences

### Positive
- The root context stays lean regardless of tool-output size — the structural
  fix for context rot, not just trimming.
- Generalizes `execute_script`'s offloading to all tools; composes with existing
  token-minimization and the #1047 prompt guidance.
- Enables large-artifact workflows (big logs/files) that today blow the window.

### Negative
- New stateful per-session store (memory, eviction, lifecycle) and two new tool
  surfaces (`read_handle`, `handle()` builtin).
- The model must learn the handle pattern; without the #1047-style guidance it
  may just `read_handle` everything back into context, negating the benefit.
- The highest-value path (in-sandbox consumption) is gated on the
  execute-script-in-agent-loop prerequisite.
- Only pays off for large outputs; pure overhead for small ones (hence the
  threshold + default-off).

## Follow-up implementation issues (create on acceptance)
1. Session-scoped handle store (type, ids, LRU/size cap, expired-handle error).
2. Handle envelope + producer opt-in for `read_file`/`exec`/`search` behind the
   threshold and config.
3. `read_handle` tool (bounded slice).
4. `execute_script` `handle(id)` builtin (needs the agent-loop prerequisite).
5. Observability attributes + privacy-corpus extension.

## References
- RLM research findings: `docs/research/rlm-harness-evaluation.md`
- ADR-006 (observability / privacy boundary), ADR-002 (compaction)
