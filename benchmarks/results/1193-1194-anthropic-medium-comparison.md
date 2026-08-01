# Vikunja 1193/1194 — Anthropic Opus 4.8 benchmark

## Method

- Provider/model: direct Anthropic API, `claude-opus-4-8`
- Thinking: `medium`
- Three complete 11-task passes per side
- Baseline: `c3d103a` plus only the Anthropic signed-thinking replay fix
- Candidate: uncommitted Vikunja 1193/1194 implementation
- Correctness gate: all 66 task-runs passed

The earlier OpenAI baseline is retained as historical data but is not compared
here because the OpenAI account ran out of credits.

## Aggregate result

| Metric | Baseline mean | Candidate mean | Delta |
|---|---:|---:|---:|
| Total tokens | 416,652 | 427,628 | **+2.63%** |
| Input tokens | 408,043 | 419,226 | +2.74% |
| Output tokens | 8,610 | 8,401 | -2.42% |
| LLM calls | 33.0 | 34.0 | +3.03% |
| Cost | $2.2555 | $2.3062 | +2.25% |
| Wall time | 145.7s | 146.7s | +0.69% |
| Correct task-runs | 33/33 | 33/33 | unchanged |

## Per-task total tokens

| Task | Baseline mean | Candidate mean | Delta |
|---|---:|---:|---:|
| 01 read/understand | 25,446 | 25,467 | +0.08% |
| 02 search usages | 47,325 | 46,571 | -1.59% |
| 03 edit/rename | 68,446 | 58,461 | -14.59% |
| 04 explore architecture | 42,569 | 56,569 | +32.89% |
| 05 execute tests | 48,363 | 48,413 | +0.10% |
| 06 git status | 23,598 | 23,592 | -0.03% |
| 07 snapshot rollback | 64,244 | 59,616 | -7.20% |
| 08 exec cargo test | 23,721 | 23,679 | -0.18% |
| 09 exec git log | 24,729 | 37,122 | +50.12% |
| 10 exec build/check | 23,931 | 23,906 | -0.10% |
| 11 exec multi-command | 24,280 | 24,233 | -0.19% |

## Interpretation

This suite provides no evidence of token savings at the default thresholds.
No post-run artifact contained an offload, result-pruning, or argument-pruning
marker, and analytics recorded no `context:microcompact` event. The large
per-task swings track different model-selected call counts (notably tasks 04
and 09), while the aggregate candidate added exactly one mean LLM call.

The benchmark therefore validates correctness and near-neutral latency, but it
does **not** exercise the new safety boundaries. The deterministic unit and
MCP lifecycle tests remain the evidence that oversized outputs are bounded and
recoverable. A future performance benchmark should add controlled >50 KiB
tool results and a single turn whose old successful results exceed the 40k
token microcompaction budget.

## Artifacts

- Baseline aggregate: `pre-1193-1194-anthropic-medium-baseline.json`
- Candidate aggregate: `post-1193-1194-anthropic-medium.json`
- Baseline binary: `8b97ff0e19c626c25125914085500c125994c5b9afd2872f2eba255b08ac6c81`
- Candidate benchmark binary: `56714a241b7488299c5a6e7c77f1039a0ee26750605989a1d211b5133de5db39`
- Final merged binary: `fcac94cd3d5397b535b58eb7dc0824f1afff7707c13cfea92208c0bc5cfcfdd5`
