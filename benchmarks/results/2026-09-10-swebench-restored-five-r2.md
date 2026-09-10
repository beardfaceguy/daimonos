# SWE-bench restored five-instance run — R2

## Scope

Configuration matches R1:

- Daimonos commit: `da9fc5e13582922e90714e0ba88591416b4f0488`
- musl binary SHA-256:
  `e024cb9c9de3083fa5d2e80d722f52d19f9e1d1ded816577c4b5b33e9a70956f`
- OpenRouter model: `anthropic/claude-opus-4.8`
- Compaction: off
- Official SWE-bench Docker images
- Enriched mini-dataset SHA-256:
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`
- Task-set SHA-256:
  `b8949f60c948b5217a8803e7cb9d624f81aa35bb467aff9409a8f11eb3ed3832`
- Raw run:
  `benchmarks/swebench/results/20260910-120053-swebench-docker-restored-five-r2`

This is a restoration/reproducibility run, not an optimization-lineage stage.

## Results

| Instance | Tokens | LLM calls | Cost | Agent wall | Patch | Resolved |
|---|---:|---:|---:|---:|---:|---:|
| `django__django-11815` | 26,702 | 3 | $0.136650 | 6.602 s | empty | no |
| `django__django-12155` | 53,764 | 4 | $0.276360 | 8.376 s | 666 B | yes |
| `django__django-12708` | 89,167 | 6 | $0.461155 | 17.477 s | 839 B | yes |
| `sphinx-doc__sphinx-8035` | 521,539 | 25 | $2.695695 | 79.222 s | 4,446 B | yes |
| `sphinx-doc__sphinx-9367` | 56,811 | 4 | $0.293915 | 9.531 s | 785 B | yes |
| **Total** | **747,983** | **42** | **$3.863775** | **121.208 s** |  | **4/5** |

The `django__django-11815` generation ended before producing a patch. OpenRouter
returned HTTP 400 after two tool-loop calls because the provider rejected an
assistant-prefill conversation: “The conversation must end with a user
message.” The task summary correctly records `exit_code=1`, `is_error=true`,
`failed_calls=1`, and an empty patch. The evaluator excluded that patch and
resolved all four submitted non-empty patches.

There were no evaluator infrastructure failures, ambiguous failures, evaluator
errors, or leaked containers.

## R2 versus R1

- total tokens: +15.77%;
- measured cost: +15.55%;
- summed agent wall: +16.08%;
- strict correctness: 4/5 in both runs, with different failure modes for
  `django__django-11815`.

The increase again comes from `sphinx-doc__sphinx-8035`: 521,539 tokens and 25
calls versus R1's 381,756 and 16 (+36.62% tokens). Across R1 and R2 that single
instance ranges from 382k to 522k tokens, consistent with the previously
recorded 173k–503k spread.

The other successful instances remained comparatively stable:

- `django__django-12155`: +0.05% tokens;
- `django__django-12708`: +0.39%;
- `sphinx-doc__sphinx-9367`: -14.95%.

Across the two restored repetitions, strict correctness is 8/10, mean total
tokens are 697,029, mean cost is $3.603775, and mean summed agent wall is
112.815 seconds. This sample establishes that the restored harness executes and
accounts correctly, but it is too small and too strategy-sensitive for an
aggregate efficiency claim.
