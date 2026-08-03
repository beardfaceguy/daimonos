# Full-suite Anthropic tool-prefix cache benchmark

## Method

- Provider/model: direct Anthropic API, `claude-opus-4-8`
- Thinking: `medium`
- Compaction: off
- Task set: complete 11-task `agent-full11-v1` suite
- Repetitions: 3
- Correctness: 33/33 task-runs
- Source commit: `1a217e30445eb5d8d76ec8ec330c5483591ce3b6`
- Binary SHA-256: `9bb3562d987ce63832dfb36d85dced4241966975db0014fb33e719f0cf164b53`
- Feature: `DAIMONOS_AGENT_PROMPT_CACHE=on`

Runs:

- `20260803-082312-agent-context-947-full-cache-r1`
- `20260803-082605-agent-context-947-full-cache-r2`
- `20260803-082911-agent-context-947-full-cache-r3`

## Aggregate lineage

| Metric | F0 original baseline | F1 after 1193/1194 | F2 prompt cache | F2 vs F1 | F2 vs F0 |
|---|---:|---:|---:|---:|---:|
| Mean total tokens | 416,652 | 427,628 | **402,699** | **-5.83%** | **-3.35%** |
| Mean fresh input | 408,043 | 419,226 | **56,504** | **-86.52%** | **-86.15%** |
| Mean cache write | 0 | 0 | 3,517 | — | — |
| Mean cache read | 0 | 0 | 334,115 | — | — |
| Mean output tokens | 8,610 | 8,401 | 8,563 | +1.93% | -0.54% |
| Mean LLM calls | 33.0 | 34.0 | **32.0** | -5.88% | -3.03% |
| Mean cost | $2.2555 | $2.3062 | **$0.6856** | **-70.27%** | **-69.60%** |
| Mean wall time | 145.7s | 146.7s | **156.0s** | **+6.36%** | **+7.09%** |
| Correct task-runs | 33/33 | 33/33 | 33/33 | unchanged | unchanged |

## F2 pass totals

| Pass | Total tokens | Fresh input | Cache write | Cache read | Output | Calls | Cost | Wall | Correct |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| r1 | 386,911 | 51,448 | 10,551 | 316,530 | 8,382 | 31 | $0.6910 | 152s | 11/11 |
| r2 | 432,109 | 64,521 | 0 | 358,734 | 8,854 | 34 | $0.7233 | 168s | 11/11 |
| r3 | 389,077 | 53,542 | 0 | 327,081 | 8,454 | 31 | $0.6426 | 148s | 11/11 |

## Per-task token lineage

Every upward blip is retained.

| Task | F0 | F1 | F2 | F2 vs F1 | F2 vs F0 |
|---|---:|---:|---:|---:|---:|
| 01 read/understand | 25,446 | 25,467 | 25,440 | -0.10% | -0.02% |
| 02 search usages | 47,325 | 46,571 | 46,694 | **+0.26%** | -1.33% |
| 03 edit/rename | 68,446 | 58,461 | 63,923 | **+9.34%** | -6.61% |
| 04 explore architecture | 42,569 | 56,569 | 46,696 | -17.45% | **+9.69%** |
| 05 execute tests | 48,363 | 48,413 | 48,667 | **+0.52%** | **+0.63%** |
| 06 git status | 23,598 | 23,592 | 23,505 | -0.37% | -0.40% |
| 07 snapshot rollback | 64,244 | 59,616 | 50,871 | -14.67% | -20.82% |
| 08 exec cargo test | 23,721 | 23,679 | 24,191 | **+2.16%** | **+1.98%** |
| 09 exec git log | 24,729 | 37,122 | 24,730 | -33.38% | **+0.00%** |
| 10 exec build/check | 23,931 | 23,906 | 23,927 | **+0.09%** | -0.02% |
| 11 exec multi-command | 24,280 | 24,233 | 24,057 | -0.72% | -0.92% |

## Per-task latency regressions versus F1

The aggregate cost/token result is strong, but latency is mixed:

- Task 02: +19.05%
- Task 03: +24.32%
- Task 05: +32.43%
- Task 08: +35.00%
- Task 10: +28.00%

Other tasks were flat or faster. The suite-level wall increase was 6.36%.

## Conclusion

Prompt caching reduced fresh input by approximately 86.5%, total tokens by
5.8%, and cost by 70.3% versus F1 while preserving correctness. It also
reduced average model calls from 34 to 32.

The latency increase and several small per-task token regressions argue against
silently enabling it without an explicit product decision. The implementation
remains default-off and opt-in while these trade-offs are reviewed.
