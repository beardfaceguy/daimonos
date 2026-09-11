# SWE-bench outlier reruns with tool traces

## Scope

Two dominant cached-Daimonos full-50 cost outliers were rerun once after
per-instance SQLite tool-trace retention landed:

- `sphinx-doc__sphinx-8638`
- `sphinx-doc__sphinx-9229`

The reruns used the same OpenRouter model
`anthropic/claude-opus-4.8`, explicit prompt caching, official SWE-bench Docker
images, compaction off, and musl binary
`0c8843a01605111777737518e17161e6e65526802ad7a9d017dc37ecf017c316`.
Only the Python benchmark runner changed: each instance now preserves the
closed analytics database as a private standalone tool trace.

## Results

| Instance | Run | Tokens | Calls | Cost | Agent wall | Tool rows | Resolved |
|---|---|---:|---:|---:|---:|---:|:---:|
| `sphinx-8638` | full-50 | 4,551,795 | 71 | $3.602758 | 452.373s | unavailable | yes |
| `sphinx-8638` | traced rerun | 582,623 | 24 | $0.735956 | 139.370s | 23 | yes |
| `sphinx-9229` | full-50 | 5,539,683 | 84 | $5.765896 | 569.893s | unavailable | yes |
| `sphinx-9229` | traced rerun | 3,238,428 | 53 | $2.813364 | 440.579s | 69 | **no** |

`sphinx-8638` rerun deltas were tokens -87.20%, calls -66.20%, cost -79.57%,
and wall -69.19%, with correctness preserved.

`sphinx-9229` rerun deltas were tokens -41.54%, calls -36.90%, cost -51.21%,
and wall -22.69%, but the patch failed. Those reductions are not
correctness-gated savings.

The two reruns cost $3.549320. Known attributable spend for the restoration,
cache comparison, full-50 comparison, and these diagnostics is $54.914191,
plus the unrecoverable first mini `sphinx-8548` attempt. The planning total
with its $3 reserve is $57.914191, below the approved $70 cap. That reserve is
not a provable bound because mini checks its threshold before the next request.

## Trace findings

### `sphinx-8638`

The 23 rows show a normal bounded edit/test progression:

- 8 searches;
- 7 reads;
- 3 edits;
- 3 direct exec calls;
- 2 git calls.

There is no repeated identical-tool pattern or retry storm. The original
71-call full-50 trajectory did not reproduce; this outlier is primarily
stochastic based on current evidence.

### `sphinx-9229`

The 69 rows show a long iterative debugging workflow:

- 15 reads and 5 searches;
- 10 direct edits;
- 14 script file writes;
- 18 script exec calls, including 14 Python executions;
- 3 direct exec calls and 2 script searches;
- 2 git calls.

Tool rows exceed the 53 model calls because `execute_script` records its nested
operations individually. The sequence repeatedly writes/runs temporary Python
checks and revises the implementation, but it is not an identical-call loop:
the existing novelty-based loop detector would not be expected to trip.

## Interpretation

The full-50 cost concentration is not a stable fixed overhead:
`sphinx-8638` returned to a normal-cost successful trajectory on rerun.
`sphinx-9229` remained expensive and became incorrect despite fewer calls.

The next change should not special-case these tasks or lower a global
tool-repeat threshold. Better candidates are:

1. retain richer privacy-safe argument/result fingerprints so novelty and
   repeated debugging paths can be distinguished;
2. add a configurable per-instance benchmark cost/wall guard so one stochastic
   trajectory cannot dominate a run;
3. investigate intra-run compaction for long one-shot tool loops.

No second full-50 repetition is justified before one of those bounded
diagnostic or safety changes lands.

## Artifacts

- `swebench/results/20260911-154212-swebench-docker-outlier-trace-8638`
- `swebench/results/20260911-154722-swebench-docker-outlier-trace-9229`
- `daimonos-anthropic__claude-opus-4.8.20260911-154212-swebench-docker-outlier-trace-8638.json`
- `daimonos-anthropic__claude-opus-4.8.20260911-154722-swebench-docker-outlier-trace-9229.json`
