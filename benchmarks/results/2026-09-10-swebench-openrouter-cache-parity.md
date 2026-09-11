# OpenRouter prompt-cache parity on SWE-bench

## Result

Explicit OpenRouter prompt caching reduced Daimonos cost by **66.98%** on the
14 instance-repetitions where both the cached and uncached patches resolved:

- uncached Daimonos: 1,577,361 tokens, 94 calls, **$8.174165**, 262.242s;
- cached Daimonos: 1,747,557 tokens, 103 calls, **$2.698824**, 322.296s.

The cost reduction is correctness-gated, but the other metrics regressed:
cached runs used 10.79% more raw tokens, made 9.57% more calls, and took 22.90%
more agent wall time. Across all attempts, uncached Daimonos resolved 15/15
while cached Daimonos resolved 14/15; the cached failure is retained below.

Caching also closed the previous 3.5x mini-swe-agent cost gap. On the 13
instance-repetitions where both cached Daimonos and mini-swe-agent resolved:

- cached Daimonos: 1,692,783 tokens, 99 calls, **$2.579491**, 313.548s;
- mini-swe-agent: 1,634,520 tokens, 201 calls, **$2.479946**, 612.259s.

Cached Daimonos cost 4.01% more while making 50.75% fewer calls and taking
48.79% less agent wall time. Raw tokens were 3.56% higher. As-run totals,
including each arm's different unresolved sample, were $3.117109 for cached
Daimonos and $2.948874 for mini-swe-agent (Daimonos 5.71% higher); both resolved
14/15.

## Scope and identity

All cold repetitions used the same five instances:

- `django__django-11815`
- `django__django-12155`
- `django__django-12708`
- `sphinx-doc__sphinx-8035`
- `sphinx-doc__sphinx-9367`

Comparison fingerprint:

- task-set SHA-256:
  `3cc9e346383b65ff688e1e7987143acde96ea1fc1097ae59e3e2384a78a83fd5`;
- enriched 50-instance dataset SHA-256:
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`;
- evaluator: SWE-bench 5.0.2 official Docker images;
- provider and exact model: OpenRouter `anthropic/claude-opus-4.8`;
- cost source: OpenRouter's per-generation `usage.cost`;
- thinking: provider/model default;
- compaction: off;
- worker count: one;
- cache mode: explicit ephemeral content-block breakpoint on the latest
  cacheable OpenRouter message;
- cold-run separation: more than the five-minute ephemeral cache TTL.

Cached binary:

- commit: `780b272807b0df77cac796ae803dc677692b3698`;
- musl SHA-256:
  `0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316`.

Uncached baseline:

- agent commit: `827d461c006d0d91af7e6a4b9c94ef2d54549534`;
- musl SHA-256:
  `bfd61ce4edea4e118c3a1a56fd7e250a31dd60bc89b9a4d5d2c8538bbc0983c2`.

## Cold-run aggregates

| Mode | Rep | Tokens | Calls | Cost | Agent wall | Resolved |
|---|---:|---:|---:|---:|---:|---:|
| uncached | 1 | 483,581 | 32 | $2.521245 | 94.638s | 5/5 |
| uncached | 2 | 668,154 | 38 | $3.456970 | 104.504s | 5/5 |
| uncached | 3 | 839,498 | 44 | $4.339410 | 125.395s | 5/5 |
| cached | 1 | 743,265 | 42 | $1.089280 | 135.884s | 5/5 |
| cached | 2 | 639,945 | 38 | $0.935098 | 111.870s | 4/5 |
| cached | 3 | 749,785 | 43 | $1.092731 | 143.857s | 5/5 |
| mini | 1 | 755,633 | 88 | $1.090288 | 267.544s | 5/5 |
| mini | 2 | 543,229 | 70 | $0.922993 | 234.902s | 4/5 |
| mini | 3 | 653,770 | 80 | $0.935593 | 225.661s | 5/5 |

Mean per five-instance cold run:

| Mode | Tokens | Calls | Cost | Agent wall |
|---|---:|---:|---:|---:|
| uncached Daimonos | 663,744 | 38.0 | $3.439208 | 108.179s |
| cached Daimonos | 710,998 | 41.0 | $1.039036 | 130.537s |
| mini-swe-agent | 650,877 | 79.3 | $0.982958 | 242.702s |

The cached Daimonos prompt total comprised 246 fresh tokens, 261,077 cache-write
tokens, and 1,849,292 cache-read tokens. The three cold runs had zero automatic
settlement retries.

## Correctness and pairing

Cached Daimonos failed `sphinx-doc__sphinx-8035` in cold repetition two.
mini-swe-agent failed `django__django-11815` in repetition two. Because these
are different samples:

- cache-versus-uncached uses 14 paired resolved samples, removing the cached
  Sphinx failure from both Daimonos arms;
- cached-Daimonos-versus-mini uses 13 paired resolved samples, removing both
  arms of both unresolved pairs.

No failed sample contributes to a savings or parity percentage.

## Retained regressions

Mean cached-versus-uncached increases by task:

- `django__django-11815`: tokens +8.18%, calls +8.33%, output +6.22%, wall
  +8.56%;
- `django__django-12155`: wall +2.19%;
- `django__django-12708`: tokens +50.56%, calls +38.89%, output +141.94%, wall
  +104.02%;
- `sphinx-doc__sphinx-8035`: tokens +1.06%, calls +5.17%, output +6.53%, wall
  +9.83%, plus one correctness failure;
- `sphinx-doc__sphinx-9367`: no upward mean metric.

Cost decreased for every task despite those stochastic trajectory increases.
The feature therefore improves charged cost, not model determinism, raw-token
use, call count, or latency.

## Warm-cache observation

One additional run started inside repetition one's cache TTL and is deliberately
excluded from the cold lineage:

- run: `20260910-171731-swebench-docker-openrouter-cache-five-r2`;
- 554,159 tokens, 35 calls, $0.485256, 97.607s, 4/5 resolved;
- cache composition: 70 fresh, 12,268 write, 536,216 read, 5,605 output.

This shows the potential cross-session warm-cache floor, but mixing it into the
cold mean would overstate reproducible savings. Its unresolved task was also
`sphinx-doc__sphinx-8035`.

## Raw artifacts

Cached cold:

- `swebench/results/20260910-171243-swebench-docker-openrouter-cache-five-smoke-r1`
- `swebench/results/20260910-172456-swebench-docker-openrouter-cache-five-cold-r2`
- `swebench/results/20260910-173233-swebench-docker-openrouter-cache-five-cold-r3`

Cached warm, excluded:

- `swebench/results/20260910-171731-swebench-docker-openrouter-cache-five-r2`

The uncached and mini raw directories are listed in
[`2026-09-10-swebench-openrouter-three-repetitions.md`](2026-09-10-swebench-openrouter-three-repetitions.md).
All cold runs have evaluator JSON reports in `benchmarks/swebench/`.

## Full-50 decision

A full 50-instance comparison is now technically justified: caching moved
Daimonos from 3.5x mini's cost to within 4.01% on correctness-matched samples,
while retaining a large call-count and wall-time advantage. A linear estimate
is approximately $10.39 for one cached Daimonos repetition and $9.83 for one
mini repetition, but the five-instance task mix is too narrow for that to be a
budget guarantee.

Run full 50 only as a new, separately fingerprinted lineage after approving an
estimated $20–$25 OpenRouter spend for one repetition of both arms.
