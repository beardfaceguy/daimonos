# ADR-002: Context/window compaction for AgentSession

**Date:** 2026-07-12
**Status:** Accepted
**Tracks:** vikunja #962 (compaction), #964 (token-usage normalization precursor)
**Relates to:** ADR-001 (provider boundary — this ADR sharpens the neutral `Usage` semantics)

## Problem

Every turn re-sends the entire conversation (`AgentSession.messages`) as the prompt. Each model
has a fixed context window; once `system + history + new message` exceeds it, the API rejects the
request and the conversation is wedged. daimonos truncates individual oversized *tool outputs*,
but nothing bounds total history growth — a long chat or tool-heavy agent run eventually hits the
wall.

**Compaction** = when history approaches the window, replace the older turns with something much
smaller (a summary) so the whole prompt fits again, while preserving enough meaning for the
conversation to continue coherently — transparently, across all three frontends (`agent` one-shot,
`chat` REPL, `acp`/Zed).

## Decision overview

- A `CompactionPolicy` (thresholds/budget) + a `CompactionStrategy` seam in the shared
  `AgentSession` core; all frontends inherit it via `AgentConfig` (like model/safety/token_log).
- MVP ships one strategy: **Summarize** (LLM-summarize the evicted prefix into one message).
- A future **Spill** strategy (evicted chunks written to an external store, with in-context
  pointers the agent dereferences via `read_file` — MemGPT-style) will be a *config-selectable*
  alternative in the same binary, so accuracy A/B benchmarks vary only the strategy — **not a
  fork** (forks drift; the benchmark would compare builds, not strategies).
- All numeric knobs are **required config in the agent env file — no defaults in code**; daimonos
  errors out at startup if compaction config is absent (user directive).

## Measuring context size (Q1)

Use the **real measured usage from the previous turn** — both provider dialects return native
token counts in every response. No local tokenizer dependency.

- One normalized accessor: `Usage::prompt_tokens()` = tokens the prompt occupied in the window.
- Canonical `Usage` semantics (sharpens ADR-001): `input` = **non-cached** prompt tokens;
  `cache_read`/`cache_write` = cached; so `prompt_tokens() = input + cache_read + cache_write`
  holds for every provider.
- Two dialects exist (provider survey, 2026-07-11):
  - **Anthropic-native:** `input_tokens` *excludes* cached → summing is correct as-is.
  - **OpenAI-compatible** (`openrouter` provider with any `base_url`: OpenRouter, xAI/Grok,
    vLLM, Ollama `/v1`, LM Studio, llama.cpp, TGI): `prompt_tokens` *includes* cached
    (`prompt_tokens_details.cached_tokens` is a sub-detail) → parser must set
    `input = prompt_tokens − cached_tokens`, `cache_read = cached_tokens`. When cache detail is
    null/absent (most self-hosted), the sum still equals `prompt_tokens`. Fixing this parser is
    #964 (current code reads defunct top-level field names, so cache counts are silently 0).
  - Cursor is a **peer agent** (Cloud Agents API + team-level usage analytics), not an LLM
    provider — no third dialect. Its Admin API is only relevant to future cost *reporting*.
- Robustness (from the survey): usage may be **absent** (self-hosted shims) → fall back to a
  `chars/4` estimate; streaming usage may arrive in a **non-final chunk** (xAI) → capture
  last-seen, don't assume final.
- Accepted limitation: measurement is between turns, so a single turn that itself balloons past
  the window isn't caught proactively (handled by the reactive net + existing tool-output
  truncation; intra-turn compaction is future work).

## Trigger policy (Q2)

- **Budget** = `context_window − output_reservation` (the window is shared by input + output).
- **Two thresholds** (avoids per-turn thrash):
  - *High-water* (trigger): compact before the next turn when last turn's `prompt_tokens()`
    ≥ `high_water × budget`.
  - *Low-water* (target): evict oldest history until the estimated kept tail ≈
    `low_water × budget`, buying many turns before the next compaction.
- **Proactive + reactive:** check at top of turn (primary, smooth); *also* catch a
  context-length-exceeded API error → force-compact → retry the turn **once** (covers the
  between-turns blind spot). Requires classifying the overflow error distinctly in each provider
  (new provider surface, in #964).

## Configuration (agent env file; no values in code)

Compaction is agent behavior, so config lives in the **agent env file** (`agent.env`), not the
TOML `Config` — matching the existing separation (`agent_env.rs`). Per user directive there are
**no numeric defaults in code**: required keys missing ⇒ startup error naming the keys + path
(same path as existing agent-env validation, exit 2).

| Key | Required | Meaning |
|---|---|---|
| `DAIMONOS_AGENT_COMPACTION` | **always** (`on`/`off`) | master switch; forces an explicit choice |
| `DAIMONOS_AGENT_COMPACTION_HIGH_WATER` | when `on` | trigger fraction (e.g. `0.75`) |
| `DAIMONOS_AGENT_COMPACTION_LOW_WATER` | when `on` | target fraction (e.g. `0.50`) |
| `DAIMONOS_AGENT_CONTEXT_WINDOW` | when `on` | model window in tokens (replaces the hard-coded heuristic for the budget) |
| `DAIMONOS_AGENT_OUTPUT_RESERVATION` | when `on` | tokens reserved for the reply (replaces the `max_tokens` code default for the budget) |
| `DAIMONOS_AGENT_SUMMARY_MODEL` | optional | summarizer model; unset → `DAIMONOS_AGENT_MODEL` (a *reference* to an explicitly-set value, not a baked-in literal) |
| `DAIMONOS_AGENT_SUMMARY_PROMPT` | optional | overrides the built-in summarization prompt template |

Validation: `0 < low_water < high_water < 1`; `context_window > 0`;
`output_reservation < context_window`. **Accepted breaking change:** every
`agent`/`chat`/`acp` invocation (including Zed's) errors until its agent.env sets at least
`DAIMONOS_AGENT_COMPACTION=off`.

Config principle established: **require** values with no safe reference (numeric knobs); **allow
omission** where the fallback is another explicitly-configured value (summary model → main model)
or a non-numeric template (summary prompt). `parse_dotenv` is literal — no `${VAR}` interpolation;
referential fallbacks are implemented in code.

## Compaction boundary + tool-pair integrity (Q3)

Hard constraint: both dialects **reject** a request where a `ToolCall` is not immediately answered
by its matching `ToolResult` (tool results are User-role messages in our history). The cut must
never split a pair.

- **Eviction unit = turn**: a genuine `User(Text)` message through (not incl.) the next genuine
  `User(Text)`. "Genuine" = `content[0]` is `Text`, not `ToolResult`.
- Cut point is always the start of a genuine user-text turn; if the token target lands mid-turn,
  **round outward** (keep more). Guarantees: kept tail starts with a real user message; no kept
  `ToolResult` references an evicted `ToolCall`; no evicted `ToolCall` loses its result.
- System prompt (`AgentConfig.system`) is separate and always kept.
- Replacement: evicted turns → `strategy.compact(evicted) -> Vec<Message>`, spliced before the
  kept tail. Summarize returns **one synthetic `User` message**
  `"[Summary of earlier conversation: …]"` (User role because the first post-system message must
  be `user`; consecutive user messages are accepted by both dialects).
- Always keep the live tail: never evict the most recent complete turn or the in-flight message.
- **MVP punt:** a single turn bigger than the whole budget can't be helped at turn granularity —
  rely on tool-output truncation + the reactive net's clear error; log when a pass can't reach
  the target.

## SummarizeStrategy (Q4)

- One-shot LLM call on the **session's provider**: system = summarization prompt, user = the
  evicted turns rendered to a **plain-text transcript** (reuses `render_transcript`; avoids
  making the summarization request itself tool-pair/schema-valid).
- Model = `DAIMONOS_AGENT_SUMMARY_MODEL`, falling back to the main model. Temperature 0
  (benchmark reproducibility). Summary tokens count toward session usage.
- Default prompt preserves: user's goal, key decisions + rationale, files/resources touched and
  their state, important facts from tool results, open threads/next steps; drops verbatim detail.
- Failure: retry once, then **structural drop** with marker
  `"[Earlier conversation truncated — summary unavailable]"`, logged — degrade lossy, never wedge.

## Wiring (Q5)

- `CompactionPolicy` on `AgentConfig`; helpers `budget()`, `should_compact()`, `target_tokens()`.
- Hook in `AgentSession::prompt`: (1) proactive check/compact → (2) run turn → (3) reactive
  compact + single retry on overflow error.
- Cut sizing walks newest→oldest accumulating **`chars/4` estimates** per turn (real usage is
  whole-conversation only; per-message counts don't exist) until the kept tail ≈ target, then
  rounds to a turn boundary. Trigger stays exact; only the cut is estimated. Tokenizer deferred
  unless accuracy proves insufficient.
- The `CompactionStrategy` seam ships now; the `DAIMONOS_AGENT_COMPACTION_STRATEGY` selector key
  is **not** exposed until a second strategy (Spill) exists — no dead one-value knob.
- Frontends inherit for free: `AgentEnv` → policy → `run_agent`/`run_chat`/`run_acp` →
  `build_agent_config` → `AgentConfig`.

## Surfacing + persistence (Q6)

Compaction only changes what daimonos **sends to the model**; it never rewrites what clients
already displayed (Zed keeps its rendered thread; the terminal keeps scrollback). Surfacing is
informational:

- **Chat REPL:** one line — `[context compacted — summarized N older turns]`.
- **ACP/Zed:** no compaction `SessionUpdate` exists in schema 1.4.0 (verified) → emit a subtle
  `AgentThoughtChunk` notice (renders as a collapsed thought).
- **Resume:** `SessionStore` persists the **already-compacted** history (compaction rewrites
  `messages` in place before the turn, and the existing post-turn save picks it up), so a resumed
  thread replays summary + recent turns — and cannot re-expand past the window. The original
  full history is intentionally not retained under Summarize (Spill will retain evicted detail
  in its chunk store).
- **Observability:** one structured JSON event per compaction on the existing `--debug-tokens`
  `token_log` channel: turns evicted, est. tokens before/after, summary model, strategy, whether
  the drop fallback fired. This is the data source for the future A/B benchmark.

## Testing (Q7)

- Pure unit: boundary logic (never splits a pair; genuine-user-turn cuts; outward rounding;
  tool-heavy/consecutive-tool/thinking/monster-turn/nothing-evictable cases); policy math;
  estimator; agent-env parsing + validation errors; overflow-error classification per provider.
- MockProvider integration on `prompt()`: proactive fires over high-water; reactive compacts and
  retries exactly once; disabled never compacts; summary call uses summary_model at temp 0;
  summarizer failure → drop-with-marker; post-compaction history stays API-valid.
- Persistence round-trip (compact → save → load = compacted form); token_log event emitted.
- Empirical smoke: real binary with a tiny `DAIMONOS_AGENT_CONTEXT_WINDOW` so compaction fires
  within 1–2 turns.
- Out of scope for the suite: the **accuracy** A/B (summarize vs spill on real tasks) — a
  separate evaluation this design enables but does not contain.

## Implementation plan

1. **PR: token-usage normalization (#964, precursor)** — canonical `Usage` semantics +
   `prompt_tokens()`; fix OpenAI-compatible cache-field parsing; drop deprecated
   `stream_options.include_usage`; position-tolerant streaming usage capture; context-overflow
   error classification in both providers.
2. **PR: compaction MVP (#962)** — policy + required-config parsing/validation/errors; strategy
   seam + SummarizeStrategy; boundary logic; `prompt()` proactive/reactive hook; frontend
   threading; surfacing; compaction event log.

## Future work (explicitly out of scope)

- **SpillStrategy** (external chunk store + in-context pointers, agent retrieves via
  `read_file`) + the `DAIMONOS_AGENT_COMPACTION_STRATEGY` selector, for the accuracy benchmark.
- Local tokenizer for cut sizing, if `chars/4` proves too coarse.
- Intra-turn compaction for single-turn overflow.
- Unified cost reporting (OpenRouter `/generation` audit; Cursor Admin API spend).
- Env-configurable `max_tokens` (the API output cap itself).
