# Research spike: RLM harness ideas for Daimonos

**Status:** findings (research/spike — no implementation in this deliverable)
**Vikunja:** #1044 / #58 (project 30)
**Source:** Alex Zhang & Omar Khattab, *"Language model harnesses are
compositional generalizers"* (MIT CSAIL, Jul 2026) —
<https://alexzhang13.github.io/blog/2026/harness/>; reference implementation
<https://github.com/alexzhang13/rlm>.

## TL;DR

The paper argues a harness's fundamental job is to shape every individual LM
call to be **locally in-distribution (LID)** — each prompt close to the model's
training distribution even when the overall task is out-of-distribution. Growing
single-context harnesses (Claude Code, Codex, ReAct) violate LID by flooding one
context with interleaved outputs ("context rot"). Their Recursive Language Model
(RLM) harness restores LID via two mechanisms: **context offloading** (input held
as a symbolic REPL variable the root LM never sees) and **programmatic
sub-agent calling** (tools/sub-agents are functions in a stateful code REPL;
intermediate outputs live in variables, not the transcript). Empirically, an RLM
root LM trained only on short tasks generalizes to 8–32× longer tasks and
transfers across domains, with eval lift matching/exceeding train lift; training
overhead is 1.5–3× (a *training* cost, not inference).

Daimonos already embodies a partial version of the second mechanism via
`execute_script`. This spike maps the paper's ideas onto Daimonos, records
keep/skip decisions, and files three scoped follow-ups.

## How Daimonos stands today (grounding)

- **`execute_script` (Starlark)** runs a script whose intermediate values stay
  in-sandbox; only the `result` variable is returned to the model. This is a
  real instance of "keep task-specific data out of the root context" — the
  closest existing analogue to the RLM REPL. Gaps vs. the paper:
  - The sandbox is **one-shot**: each call builds a fresh Starlark module
    (`eval_module`), so variables do **not** persist across `execute_script`
    calls. The RLM REPL is stateful across turns (variables accumulate).
  - `result` is **returned verbatim** — a large `result` still floods context.
  - There is **no LLM sub-call primitive** in the sandbox (no analogue of
    `llm_query_batched`); Starlark builtins are the tool wrappers only.
  - `execute_script` is **not reachable from the internal agent loop**
    (`agent::run`) today — it is dispatched only on the MCP-server path (it has
    `to_request: None`, and the agent loop serves opcode tools + remote MCP).
    Same prerequisite blocking #1045.
- **Direct tool calls** (`read_file`, `exec`, `search`, …) inline their full
  payload into the model context. Token-minimization (plugin redirect, exec
  output filtering, read dedup, compact response fields) trims this but still
  inlines — a lightweight, LID-adjacent measure, not offloading.
- **Compaction (ADR-002)** reactively/proactively summarizes old turns once
  context grows — a *recovery* from context growth, complementary to LID which
  *prevents* it.
- **Observability (ADR-006, #1039/#1040)** now traces prompt → generation →
  tool.call/mcp.remote_tool → context.compaction/agent.retry with usage, cost,
  TTFT, durations, and token/savings estimates — the instrumentation needed to
  measure any LID/offload change empirically.

## Evaluation by angle

### 1. Context offloading as a first-class primitive — **KEEP (design)**

Extend offloading beyond `execute_script`: let a tool call optionally return a
compact **handle** + metadata (size/kind/head-or-summary) instead of the full
body, binding the payload to a referenceable value the model never sees inlined,
which it can later feed into `execute_script`/another tool. This is the direct
analogue of RLM binding input to a symbolic `context` variable, and it
generalizes Daimonos's existing token-minimization from "trim the inline" to
"don't inline at all." Non-trivial (handle store, lifecycle, Starlark accessor,
opt-in threshold, observability), so it warrants an ADR before code.
→ **#1048** (Design: context-offload handles for tool outputs).

### 2. Programmatic LLM sub-calls in the sandbox — **KEEP (ADR first)**

The highest-leverage mechanism (drove the ~10× eval lift): a bounded
`llm_query_batched(prompts) -> outputs` inside Starlark so the agent fans out
extraction/classification/summarization over chunks whose intermediate outputs
never touch the root context. Heaviest to build safely and gated on real
prerequisites — `execute_script` must first be reachable under a prompt root
with a provider handle (same blocker as #1045), plus budget/fan-out/recursion/
timeout/cancellation controls, cost roll-up, and per-sub-call `llm.generation`
spans nested under the script's `tool.call` (exactly the nesting deferred in
#1045). ADR before implementation.
→ **#1049** (ADR: programmatic LLM sub-calls in execute_script). Depends on #1045.

### 3. Harness/prompt guidance toward decomposition — **KEEP (cheap, ship soon)**

The paper shows models default to the degenerate "offload the whole task to one
sub-call" without a decomposition nudge, which breaks LID. Daimonos already
nudges toward `execute_script` for multi-step work but never says "operate on
large data in-sandbox and return a compact summary, not the raw dump." Adding
that guidance (in `prompts/*.md`, provider-neutral, unit-tested) is a
zero-architecture win available immediately and independent of #1048/#1049.
→ **#1047** (Prompt guidance: prefer offload-then-summarize).

### 4. Overhead trade-off — **SKIP as standalone; fold into #1049**

The paper's 1.5–3× is a **training** (RL) overhead — not applicable to Daimonos,
which is an inference-time harness with no training. The inference analogue is
extra round-trip latency (sub-calls/recursion) vs. context-token savings. There
is nothing to build here in isolation; the observability spans from #1039/#1040
(generation TTFT/usage/cost, tool.call durations) already provide the
measurement surface, so quantification belongs inside the #1049 sub-call work
once there is something to measure.

### 5. LID for the future bare-metal OS interface — **KEEP as a design tenet**

The paper has no OS interface, but the LID principle transfers cleanly: a
syscall / StructFS observation interface should return **structured, bounded,
offload-by-reference** observations (paginated, summarized, handle-backed)
rather than raw blobs, so each observation is *LID by construction* rather than
by after-the-fact trimming. Recorded here as a design tenet for the eventual OS
work; no ticket created (no active OS-design track to attach it to). Carry this
into that ADR when it happens: **observations should be LID by construction.**

## Cross-cutting observation

The three "keep" items form a natural progression that also unblocks the
deferred observability follow-up: prompt guidance (#1047, now) → offload handles
(#1048) and sub-calls (#1049), both of which require `execute_script` reachable
under a prompt root (#1045) and would produce the nested `tool.call` → child
`llm.generation` span tree that #1045 anticipates. The observability work already
done is the measurement backbone for evaluating whether any of this actually
reduces context and improves outcomes.

## Decisions summary

| Angle | Decision | Follow-up |
|-------|----------|-----------|
| 1. Context-offload handles | Keep (design/ADR) | #1048 |
| 2. Programmatic LLM sub-calls | Keep (ADR first; needs #1045) | #1049 |
| 3. Decomposition prompt guidance | Keep (cheap, ship soon) | #1047 |
| 4. Overhead quantification | Skip standalone; fold into #1049 | — |
| 5. LID-by-construction OS observations | Keep as design tenet | recorded here |

## Caveats

- The RLM's headline results are from **RL-training** a root LM; Daimonos uses
  off-the-shelf frontier models with no training, so the *generalization* claims
  transfer only as an architectural inductive bias, not a guaranteed lift.
- The paper acknowledges leakage (the trained RLM still sometimes prints
  task-specific info back to the main context) and that some supervision/hint is
  useful for convergence — reinforcing that #1047 (guidance) is a real lever, not
  a nicety.
- No numbers here are Daimonos-measured; empirical validation is deferred to the
  implementation follow-ups using the existing OTLP spans.
