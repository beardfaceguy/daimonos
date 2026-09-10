# SWE-bench `sphinx-8035` trajectory analysis

## Finding

The R1/R2 token spread is model-strategy variance, not retry or evaluator
failure:

| Metric | R1 | R2 | Change |
|---|---:|---:|---:|
| Total tokens | 381,756 | 521,539 | +36.62% |
| LLM calls | 16 | 25 | +9 |
| Tool-loop calls | 15 | 24 | +9 |
| Cost | $1.977600 | $2.695695 | +36.31% |
| Agent wall | 58.109 s | 79.222 s | +36.33% |
| Final context estimate | 20,755 tokens | 16,013 tokens | -22.85% |
| Patch size | 3,922 B | 4,446 B | +13.36% |
| Correctness | resolved | resolved | unchanged |

Every provider call succeeded. Neither run recorded a failed generation or tool
result error. R1 used one two-operation `execute_script`; R2 used two, totaling
four operations. The additional nine R2 calls therefore represent a longer
successful exploration/verification path, not retries or a stuck identical-call
loop.

Prompt input dominates both runs: 378,315 of 381,756 tokens in R1 and 517,139
of 521,539 in R2. Each extra generation re-sends fixed system and tool-schema
context. R2's context was smaller per call and at the final call, but 25 calls
overwhelmed that per-call reduction.

Both patches implement the same core change: parse `private-members` with
`members_option` and filter explicit private-member names. R2 additionally
updates `CHANGES` and uses slightly different branching style. Both pass the
official evaluator, so the extra R2 work did not change measured correctness.

## Observability gap

The Docker runner preserves token summaries, final text, stderr, and patches,
but not Daimonos's per-tool analytics database. Exact tool names and arguments
for the 15/24 tool loops are therefore unavailable after the container is
removed. Context-component deltas can distinguish successful tool-loop growth
from retries, but cannot identify which reads/searches/tests were repeated.

Before optimizing this trajectory, the runner should mount a private per-task
analytics directory and export an ordered, content-safe tool-call trace. That
would separate useful exploration from redundant reads without retaining source
or tool-result contents.

## Conclusion

Do not optimize from aggregate R1/R2 token totals alone. The actionable signal
is call-count variance on `sphinx-8035`; the missing tool trace is the next
measurement gap. Any intervention needs a matching baseline with repeated runs
and must preserve this instance's per-run regressions.
