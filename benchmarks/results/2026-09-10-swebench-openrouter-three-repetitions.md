# SWE-bench OpenRouter comparison: three matched repetitions

## Result

Under each harness's default behavior, mini-swe-agent was cheaper than
Daimonos on the same OpenRouter model and the same OpenRouter-reported cost
field.

The primary correctness-gated comparison contains the 14 paired samples where
both patches resolved. It excludes both arms of the unresolved
repetition-two `django__django-11815` pair:

- Daimonos: 1,936,396 tokens, 110 calls, **$10.034260**, 315.225 seconds.
- mini-swe-agent: 1,914,875 tokens, 229 calls, **$2.857297**, 703.297 seconds.
- mini-swe-agent cost **71.52% less**, or Daimonos cost **3.51x** as much.
- Daimonos used 1.12% more raw tokens, 51.97% fewer calls, and 55.18% less
  agent wall time.

The secondary as-run totals include mini-swe-agent's unresolved sample:

- Daimonos: 1,991,233 tokens, 114 calls, **$10.317625**, 324.537 seconds,
  15/15 resolved.
- mini-swe-agent: 1,952,632 tokens, 238 calls, **$2.948874**, 728.107 seconds,
  14/15 resolved.
- mini-swe-agent cost **71.42% less**, or Daimonos cost **3.50x** as much.

Daimonos cost more on every one of the 15 paired attempts. It used more raw
tokens on 9/15, took more wall time on 1/15, and never made more calls. The
failed mini sample is retained in the table rather than hidden.

## Scope

The five instances in every repetition were:

- `django__django-11815`
- `django__django-12155`
- `django__django-12708`
- `sphinx-doc__sphinx-8035`
- `sphinx-doc__sphinx-9367`

Comparison fingerprint:

- dataset: `MariusHobbhahn/swe-bench-verified-mini`, enriched fingerprint
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`;
- evaluator: SWE-bench 5.0.2 official Docker images;
- provider: OpenRouter;
- exact model: `anthropic/claude-opus-4.8`;
- cost source for both arms: sum of OpenRouter's per-generation `usage.cost`;
- mini cost cross-check: all 15 sums matched
  `model_stats.instance_cost`;
- mini-swe-agent: 2.4.6 on CPython 3.12.14;
- Daimonos agent code: commit
  `827d461c006d0d91af7e6a4b9c94ef2d54549534`;
- Daimonos musl binary:
  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`;
- repetitions: three, run sequentially with one worker.

The patch collector changed during repetition three only after a paid turn
failed to produce a prediction. The recovery change did not alter the agent,
model request, prompt, or binary.

## Repetition aggregates

| Rep | Harness | Tokens | Calls | OpenRouter cost | Agent wall | Resolved |
|---:|---|---:|---:|---:|---:|---:|
| 1 | Daimonos | 483,581 | 32 | $2.521245 | 94.638s | 5/5 |
| 1 | mini | 755,633 | 88 | $1.090288 | 267.544s | 5/5 |
| 2 | Daimonos | 668,154 | 38 | $3.456970 | 104.504s | 5/5 |
| 2 | mini | 543,229 | 70 | $0.922993 | 234.902s | 4/5 |
| 3 | Daimonos | 839,498 | 44 | $4.339410 | 125.395s | 5/5 |
| 3 | mini | 653,770 | 80 | $0.935593 | 225.661s | 5/5 |

Daimonos cost 2.31x, 3.75x, and 4.64x mini-swe-agent in repetitions 1–3.
mini's as-run spend was stable ($0.923–$1.090), though repetition two includes
one unresolved sample. Daimonos varied from $2.521 to $4.339, primarily because
`sphinx-doc__sphinx-8035` took progressively more turns.

Mean per five-instance repetition:

| Harness | Tokens | Calls | Cost | Agent wall | Cost standard deviation |
|---|---:|---:|---:|---:|---:|
| Daimonos | 663,744 | 38.0 | $3.439208 | 108.179s | $0.909213 |
| mini | 650,877 | 79.3 | $0.982958 | 242.702s | $0.093163 |

## Why raw tokens and charged cost diverge

mini-swe-agent enabled `set_cache_control=default_end`. Its 1,907,441 prompt
tokens plus 45,191 completion tokens reconcile to the 1,952,632 total. Prompt
tokens consisted of:

- 1,756,837 cache reads (92.10%);
- 150,128 cache writes (7.87%);
- 476 fresh input tokens (0.02%).

Daimonos reported no cache reads or writes, so all 1,973,160 prompt tokens were
fresh; its 18,073 completion tokens produce the 1,991,233 total.

OpenRouter documents `cached_tokens` and `cache_write_tokens` as prompt-token
subsets ([prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching),
[usage accounting](https://openrouter.ai/docs/cookbook/administration/usage-accounting)).
The normalizer rejects a response if those subsets exceed `prompt_tokens`
rather than silently undercounting fresh input. The 92.10% cache-read ratio is
descriptive of this workload and call count; each instance's first generation
must establish a cache entry before later calls can hit it.

OpenRouter charges cached input differently from fresh input. The result
therefore answers the requested default-harness question—what each harness cost
as configured—but does not isolate caching from other harness differences.

The conclusion is not solely a `sphinx-doc__sphinx-8035` artifact. Removing
that high-variance task and the unresolved pair leaves 11 correctness-matched
samples: Daimonos cost $3.769215 versus mini-swe-agent's $1.526945, so mini
still cost 59.49% less (Daimonos 2.47x).

## Per-sample results

| Rep | Instance | D tokens | mini tokens | D cost | mini cost | D wall | mini wall | D | mini |
|---:|---|---:|---:|---:|---:|---:|---:|:---:|:---:|
| 1 | `django__django-11815` | 54,780 | 55,375 | $0.282880 | $0.122261 | 9.263s | 34.440s | pass | pass |
| 1 | `django__django-12155` | 53,764 | 38,968 | $0.276360 | $0.082543 | 9.934s | 23.023s | pass | pass |
| 1 | `django__django-12708` | 89,435 | 243,253 | $0.464875 | $0.347552 | 16.505s | 87.217s | pass | pass |
| 1 | `sphinx-doc__sphinx-8035` | 205,440 | 399,300 | $1.083820 | $0.482990 | 45.438s | 109.996s | pass | pass |
| 1 | `sphinx-doc__sphinx-9367` | 80,162 | 18,737 | $0.413310 | $0.054941 | 13.498s | 12.868s | pass | pass |
| 2 | `django__django-11815` | 54,837 | 37,757 | $0.283365 | $0.091577 | 9.312s | 24.810s | pass | **fail** |
| 2 | `django__django-12155` | 53,766 | 32,256 | $0.276410 | $0.072563 | 8.316s | 20.592s | pass | pass |
| 2 | `django__django-12708` | 88,858 | 171,623 | $0.459570 | $0.312987 | 14.629s | 87.258s | pass | pass |
| 2 | `sphinx-doc__sphinx-8035` | 413,872 | 280,355 | $2.143460 | $0.377351 | 62.295s | 91.038s | pass | pass |
| 2 | `sphinx-doc__sphinx-9367` | 56,821 | 21,238 | $0.294165 | $0.068516 | 9.952s | 11.204s | pass | pass |
| 3 | `django__django-11815` | 54,770 | 55,590 | $0.282710 | $0.119367 | 8.988s | 33.883s | pass | pass |
| 3 | `django__django-12155` | 53,773 | 31,552 | $0.276585 | $0.070647 | 9.443s | 20.420s | pass | pass |
| 3 | `django__django-12708` | 89,653 | 148,673 | $0.465925 | $0.221779 | 15.878s | 55.384s | pass | pass |
| 3 | `sphinx-doc__sphinx-8035` | 587,845 | 399,335 | $3.037765 | $0.470011 | 82.245s | 103.444s | pass | pass |
| 3 | `sphinx-doc__sphinx-9367` | 53,457 | 18,620 | $0.276425 | $0.053789 | 8.841s | 12.530s | pass | pass |

## Failed and excluded operations

These attempts are preserved but excluded from the correctness-gated
comparison:

- mini repetition two first launch: five authentication failures, zero model
  calls and $0.00. The retry explicitly mapped the existing agent key into
  mini-swe-agent's expected environment variable.
- Daimonos repetition three `sphinx-doc__sphinx-8035`: the agent completed, but
  strict UTF-8 decoding of a generated binary test artifact aborted patch
  collection. It consumed 793,425 tokens, 34 calls, and **$4.083385** without a
  scoreable prediction. [PR #225](https://github.com/beardfaceguy/daimonos/pull/225)
  fixed the collector and its two-instance recovery resolved 2/2.
- Daimonos repetition one `sphinx-doc__sphinx-9367`: OpenRouter rejected the
  first request with `in_flight_budget_exhausted`; it consumed zero tokens and
  $0.00 before a settled retry.

Including the paid but unscoreable patch-collection failure, actual Daimonos
spend represented by these artifacts was $14.401010. It is operational
overhead, not evidence for a correctness-gated efficiency delta.

## Run artifacts

Daimonos:

- `swebench/results/20260910-130053-swebench-docker-compare-five-r1`
- `swebench/results/20260910-130512-swebench-docker-compare-five-r1-retry9367`
- `swebench/results/20260910-135741-swebench-docker-compare-five-r2`
- `swebench/results/20260910-140920-swebench-docker-compare-five-r3`
- `swebench/results/20260910-141753-swebench-docker-compare-five-r3-retry-sphinx`

mini-swe-agent:

- `swebench/results/mini-compare-five-r1`
- `swebench/results/mini-compare-five-r2` (authentication failure)
- `swebench/results/mini-compare-five-r2-authretry`
- `swebench/results/mini-compare-five-r3`

All six scoreable arm-repetitions have SWE-bench evaluator JSON reports in the
SWE-bench directory.

## Full-50 decision

Do not start the full 50-instance comparison yet. A linear extrapolation from
the scoreable as-run means is roughly $34.39 for Daimonos and $9.83 for
mini-swe-agent per repetition; the full suite's task mix can differ
substantially. Real budgeting also needs an operational-overhead allowance:
the observed paid collector failure raises represented Daimonos spend from
$10.32 to $14.40. That fixed defect is now repaired, so neither number is a
reliable full-50 forecast.

The next useful experiment is cache parity:

1. add equivalent OpenRouter prompt-cache support to Daimonos and rerun this
   five-instance scope, or
2. disable mini-swe-agent caching and compare both uncached.

The first option answers the product-cost question; the second isolates the
harness overhead. A full-50 run should follow only after choosing one of those
scopes.
