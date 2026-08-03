# Native context composition and Anthropic tool-prefix caching

## Question

For the current native Daimonos agent loop, is repeated context cost driven
primarily by extra model calls or by larger per-call context, and which stable
component is the best optimization target?

This does not directly reproduce the historical Claude CLI/MCP 2.3×
cache-read gap: Claude owned that context and Daimonos could not observe it.
This experiment measures the current first-party agent runtime.

## Instrumentation

`--debug-tokens` now records metadata-only context composition for every
generation. The record contains only numeric byte/item counts—never prompt,
tool argument/result, image, URI, path, or provider-state content.

Measured categories:

- system prompt
- tool names, descriptions, and JSON schemas
- user and assistant text
- thinking and opaque provider continuation state
- tool-call arguments
- successful and failed tool results
- encoded images

`extract_tokens.py` aggregates actual provider prompt occupancy and estimated
component exposure. `context_compare.py` separates total prompt-token change
into call-count and mean-context effects.

## Method

- Provider/model: direct Anthropic API, `claude-opus-4-8`
- Thinking: `medium`
- Task: `04-explore-architecture`
- Four correctness-gated repetitions per arm
- Compaction: off
- Baseline binary: `7f7c16d19afcffe40c2b4be051045a9fd537aaa11809a0f094779bcf281eda9a`
- Candidate binary: `78f17525baa9bc6d73f3d459d0d4067f21d2c3ce6f045ee712678dda33a4bbc9`

Baseline runs:

- `20260801-182400-agent-context-947-baseline-r1`
- `20260801-182454-agent-context-947-baseline-r2`
- `20260801-182944-agent-context-947-baseline-r3`
- `20260801-183028-agent-context-947-baseline-r4`

Candidate runs:

- `20260801-182801-agent-context-947-candidate-r1`
- `20260801-182857-agent-context-947-candidate-r2`
- `20260801-183112-agent-context-947-candidate-r3`
- `20260801-183202-agent-context-947-candidate-r4`

All eight runs passed the task’s correctness check.

## Baseline composition

The baseline was highly stable: three calls per run and approximately 13,358
actual prompt tokens per call.

Largest estimated repeated exposures per three-call run:

| Component | Estimated tokens | Per call |
|---|---:|---:|
| Tool JSON schemas | 11,010 | 3,670 |
| Tool descriptions | 6,717 | 2,239 |
| Successful tool results | 2,870 | 957 |
| System prompt | 1,923 | 641 |
| Tool names | 288 | 96 |

Tool definitions therefore contribute roughly 5,909 estimated tokens per call
and are the largest stable reducible prefix.

## Optimization

The Anthropic request adapter now places
`cache_control: {"type":"ephemeral"}` on the final tool definition. Per
Anthropic’s documented tools → system → messages prefix hierarchy, that caches
the complete tool-definition prefix while preserving a separate breakpoint
from any changing system/coordination notice.

## Result

Across all four runs:

| Metric | Baseline | Candidate | Delta |
|---|---:|---:|---:|
| Correct runs | 4/4 | 4/4 | unchanged |
| LLM calls | 12 | 15 | +3 |
| Mean prompt tokens/call | 13,358 | 13,187 | -1.3% |
| Fresh input tokens | 160,297 | 39,543 | **-75.3%** |
| Cache-write tokens | 0 | 10,551 | +10,551 |
| Cache-read tokens | 0 | 147,714 | +147,714 |
| Cost | $1.0400 | $0.5918 | **-43.1%** |
| Wall time | 154s | 152s | -1.3% |

Symmetric prompt-token decomposition:

- total prompt-token delta: **+37,511**
- call-count effect: **+39,817.9**
- mean-context effect: **-2,306.9**

The candidate’s one six-call outlier accounts for the higher total prompt-token
volume. The mean-context effect was slightly favorable; extra turns dominated.

The three warm-cache candidate runs each used three calls and averaged
approximately **$0.1178**, versus **$0.2600** for the four baseline runs—a
roughly **54.7% cost reduction** at matched call count.

## Conclusions

1. The measured “cache-read gap” is not itself waste: replacing fresh input
   with much cheaper cache reads substantially reduced cost.
2. Tool definitions are the dominant stable native-agent prefix and explicit
   caching is effective.
3. Call-count variance remains the largest driver of total token exposure.
   Future work should explain task-specific extra turns rather than trimming
   already-cacheable static bytes.
4. Estimated neutral-context components cover only part of provider-reported
   prompt tokens; wire-format/tokenizer overhead remains in the residual. The
   component ranking is still stable across repetitions.

## Shipping posture

The measurement and comparison infrastructure is safe to retain
unconditionally. Tool-prefix caching is exposed as
`DAIMONOS_AGENT_PROMPT_CACHE=on` and defaults off until a broader native-agent
suite measures the first-request cache-write premium across one-call and
multi-call workloads.
