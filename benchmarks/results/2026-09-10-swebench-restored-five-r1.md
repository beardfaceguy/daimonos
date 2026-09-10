# SWE-bench restored five-instance run — R1

## Scope

- Runtime: Daimonos inside official SWE-bench Docker images
- Provider/model: OpenRouter, `anthropic/claude-opus-4.8`
- Compaction: off
- Daimonos commit: `da9fc5e13582922e90714e0ba88591416b4f0488`
- musl binary SHA-256:
  `e024cb9c9de3083fa5d2e80d722f52d19f9e1d1ded816577c4b5b33e9a70956f`
- Enriched mini-dataset SHA-256:
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`
- Task-set SHA-256:
  `b8949f60c948b5217a8803e7cb9d624f81aa35bb467aff9409a8f11eb3ed3832`
- Repetitions: one
- Raw run:
  `benchmarks/swebench/results/20260910-115346-swebench-docker-restored-five-r1`
- Evaluation report:
  `benchmarks/swebench/daimonos-anthropic__claude-opus-4.8.20260910-115346-swebench-docker-restored-five-r1.json`

This is a restoration/reproducibility run, not an optimization-lineage stage.
It does not establish a savings or regression claim at one repetition.

## Results

| Instance | Tokens | LLM calls | Cost | Agent wall | Patch | Resolved |
|---|---:|---:|---:|---:|---:|---:|
| `django__django-11815` | 54,967 | 4 | $0.285315 | 10.553 s | 779 B | no |
| `django__django-12155` | 53,737 | 4 | $0.275685 | 9.379 s | 666 B | yes |
| `django__django-12708` | 88,818 | 6 | $0.460830 | 15.402 s | 787 B | yes |
| `sphinx-doc__sphinx-8035` | 381,756 | 16 | $1.977600 | 58.109 s | 3,922 B | yes |
| `sphinx-doc__sphinx-9367` | 66,797 | 5 | $0.344345 | 10.978 s | 785 B | yes |
| **Total** | **646,075** | **35** | **$3.343775** | **104.421 s** |  | **4/5** |

All five patches applied and evaluated. There were no infrastructure failures,
ambiguous failures, empty patches, evaluator errors, or leaked containers.

`django__django-11815` failed one of its two FAIL_TO_PASS tests:
`test_serialize_enums`. Its patch used `enum_class.__qualname__`; the two earlier
smoke runs used `enum_class.__name__` and resolved the instance.

## Historical one-pass comparison

The prior README five-instance run reported 436,834 tokens, approximately
$1.53, 84 seconds, and 5/5 resolved. This run observed:

- tokens: +47.90%;
- measured cost: +118.55%;
- summed agent wall time: +24.31%;
- correctness: 4/5 instead of 5/5.

Four task token totals were within ±0.4% of the prior run. The aggregate increase
comes almost entirely from `sphinx-doc__sphinx-8035`, which rose from 172,729
tokens / 10 calls to 381,756 tokens / 16 calls (+121.01%). Historical notes
already record a 503k-token run for that same instance, so this is consistent
with known strategy variance rather than enough evidence for a harness
regression.

At least one additional repetition is required before interpreting aggregate
movement. Every per-task regression remains visible above.
