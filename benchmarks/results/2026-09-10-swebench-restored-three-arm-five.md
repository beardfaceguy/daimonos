# Restored SWE-bench five-instance three-arm comparison

This one-repetition restoration report is retained as the original record.
The OpenRouter cost conclusion is superseded by the
[three-repetition comparison](2026-09-10-swebench-openrouter-three-repetitions.md).

## Scope

All arms ran the same five mini-suite instances inside their official
SWE-bench images and were scored by SWE-bench 5.0.2:

- `django__django-11815`
- `django__django-12155`
- `django__django-12708`
- `sphinx-doc__sphinx-8035`
- `sphinx-doc__sphinx-9367`

Shared model family: Claude Opus 4.8. Daimonos and mini-swe-agent used the exact
OpenRouter slug `anthropic/claude-opus-4.8`; Cursor used its corresponding
`claude-opus-4-8-medium` backend model.

Versions:

- Daimonos commit `827d461c006d0d91af7e6a4b9c94ef2d54549534`
- Daimonos musl binary
  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`
- Cursor CLI `2026.09.02-c22c1a3`
- mini-swe-agent `2.4.6`
- CPython `3.12.14`
- enriched dataset
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`

Each arm has one repetition. This is a restored-harness validation, not an
optimization-lineage stage or publishable aggregate claim.

## Aggregate results

| Harness | Tokens | LLM calls | Measured cost | Agent wall | Resolved |
|---|---:|---:|---:|---:|---:|
| Daimonos | 483,581 | 32 | $2.521245 | 94.638 s | 5/5 |
| mini-swe-agent | 755,633 | 88 | $1.090288 | 267.544 s | 5/5 |
| Cursor | 1,307,205 | unavailable | unavailable | 261.703 s | 5/5 |

Daimonos used 36.00% fewer raw tokens and 64.63% less agent wall time than
mini-swe-agent, and 63.01% fewer raw tokens and 63.84% less agent wall time than
Cursor in this repetition.

Those token ratios are not cost ratios. Daimonos and mini-swe-agent costs both
sum OpenRouter's provider-returned per-generation `usage.cost`; mini's summed
value also equals its trajectory-level `model_stats.instance_cost`. In this
single, provisional matched repetition under the tested harness defaults,
mini-swe-agent therefore cost 56.76% less than Daimonos despite using more raw
tokens.

Caching explains the divergence rather than invalidating that default-harness
cost comparison: mini-swe-agent used `set_cache_control=default_end`, Cursor
reported 1,164,639 cache-read tokens, and Daimonos reported no cache reads.
A cache-controlled experiment is required to attribute the difference, but not
to answer which OpenRouter arm cost more as configured. Cursor bills through
Cursor's backend, so its USD cost remains unavailable.

## Per-instance results

| Instance | Daimonos tokens | mini tokens | Cursor tokens | D cost | mini cost |
|---|---:|---:|---:|---:|---:|
| `django__django-11815` | 54,780 | 55,375 | 90,989 | $0.282880 | $0.122262 |
| `django__django-12155` | 53,764 | 38,968 | 88,934 | $0.276360 | $0.082542 |
| `django__django-12708` | 89,435 | 243,253 | 117,593 | $0.464875 | $0.347552 |
| `sphinx-doc__sphinx-8035` | 205,440 | 399,300 | 921,135 | $1.083820 | $0.482990 |
| `sphinx-doc__sphinx-9367` | 80,162 | 18,737 | 88,554 | $0.413310 | $0.054942 |

All 15 patches resolved. No evaluator infrastructure failures, ambiguous
failures, errors, or leaked containers occurred.

## Run artifacts and caveats

- Daimonos:
  `results/20260910-130053-swebench-docker-compare-five-r1`, with
  `sphinx-9367` replaced by the budget-settled retry in
  `results/20260910-130512-swebench-docker-compare-five-r1-retry9367`.
- Cursor:
  `results/20260910-130704-swebench-cursor-docker-compare-five-r1`.
- mini-swe-agent: `results/mini-compare-five-r1`.

The first Daimonos `sphinx-9367` attempt was rejected before inference with
OpenRouter HTTP 402 `in_flight_budget_exhausted`; it consumed zero tokens and
zero cost. The replacement ran after the stated 120-second settlement window.

Cursor's extractor currently reports no LLM-call/tool-call count, so those
fields must not be treated as zero. mini-swe-agent cost comes from summing the
same OpenRouter `usage.cost` field Daimonos records; LiteLLM's aggregate is
retained only as a consistency check.

At least two more matched repetitions are needed before publishing relative
efficiency. An explicit cache-policy decision is needed only for causal
attribution, not for the harness-default cost ranking above.
