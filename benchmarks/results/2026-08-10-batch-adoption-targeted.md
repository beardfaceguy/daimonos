# Execute-script batch adoption targeted lineage

## Scope

- Provider/model: Anthropic `claude-opus-4-8`
- Thinking: `medium`
- Compaction: off
- Prompt caching: off
- Tasks: `02-search-usages`, `03-edit-rename`, `07-snapshot-rollback`
- Repetitions: 3 per task
- Task-set fingerprint:
  `1c5984aebc8455d99fc222fcddacc70b9e5cec2124ed4cac8dcad991fdda52c7`
- Fixture commit: `600221ccae548a57ac3c4542966431243bd43c6a`
- Daimonos commit: `2bb1c86d92444d64a51ce89b52e7209b3b0c987f`
- Binary SHA-256:
  `d5ad86f25829b6cff169426d9eb2fde48315e9abcdee6e80a570c7681bb3d796`
- Correctness gate: all task workspace/response checks must pass
- Batch adoption: at least one `execute_script` call with serialized arguments
  of 700 bytes or more

This is a new targeted lineage and is not compared with the full-suite lineage:
the task set and reconstructed fixture commit differ.

## B0 — Current behavior baseline

All 9 task-runs passed correctness. Batch adoption was **0/9 (0%)**.
`execute_script` appeared in two runs, but only for small scripts (maximum 240
bytes), not the multi-operation strategy under test.

| Task | Correct | Batch adoption | Mean LLM calls | Mean tokens | Mean cost | Mean wall |
|---|---:|---:|---:|---:|---:|---:|
| 02 search usages | 3/3 | 0/3 | 2.33 | 29,711 | $0.1601 | 9.0 s |
| 03 edit rename | 3/3 | 0/3 | 5.00 | 64,612 | $0.3330 | 12.7 s |
| 07 snapshot rollback | 3/3 | 0/3 | 6.67 | 89,586 | $0.4675 | 17.3 s |
| **Overall** | **9/9** | **0/9** | **4.67** | **61,303** | **$0.3202** | **13.0 s** |

Raw run directories:

- `20260810-170124-agent-1230-baseline-r1-task02`
- `20260810-170458-agent-1230-baseline-r1-task03`
- `20260810-170512-agent-1230-baseline-r1-task07`
- `20260810-170531-agent-1230-baseline-r2-task02`
- `20260810-170543-agent-1230-baseline-r2-task03`
- `20260810-170559-agent-1230-baseline-r2-task07`
- `20260810-170616-agent-1230-baseline-r3-task02`
- `20260810-170625-agent-1230-baseline-r3-task03`
- `20260810-170638-agent-1230-baseline-r3-task07`

An accidentally unfiltered, interrupted run directory is intentionally excluded
from this correctness-gated stage.

## B1 — Restrict the first generation to `execute_script`

For mutation-plus-verification tasks, the first provider generation exposed
only `execute_script`; later generations regained the full tool catalog.

All 9 task-runs passed correctness, but batch adoption remained **0/9 (0%)**.
The intervention therefore fails the primary acceptance criterion. It made the
model use a small script for initial inspection, then continue with separate
operations. No `execute_script` result errors occurred.

| Task | Correct | Batch adoption | Mean LLM calls | Mean tokens | Mean cost | Mean wall |
|---|---:|---:|---:|---:|---:|---:|
| 02 search usages | 3/3 | 0/3 | 2.00 | 25,394 | $0.1388 | 9.3 s |
| 03 edit rename | 3/3 | 0/3 | 5.00 | 55,393 | $0.2871 | 12.7 s |
| 07 snapshot rollback | 3/3 | 0/3 | 5.33 | 61,808 | $0.3281 | 17.7 s |
| **Overall** | **9/9** | **0/9** | **4.11** | **47,532** | **$0.2513** | **13.2 s** |

Immediate and cumulative delta from B0 (this targeted lineage starts at B0):

- Mean LLM calls: **-11.9%**
- Mean tokens: **-22.5%**
- Mean cost: **-21.5%**
- Mean wall time: **+1.7%**
- Batch adoption: unchanged at **0%**

The token/cost change cannot be attributed to batch adoption and is not claimed
as a reliable saving. Task 07 still varied from 4 to 6 calls, while task 03
remained fixed at 5 calls.

Raw run directories:

- `20260810-191129-agent-1230-script-first-r1-task02`
- `20260810-191139-agent-1230-script-first-r1-task03`
- `20260810-191155-agent-1230-script-first-r1-task07`
- `20260810-191213-agent-1230-script-first-r2-task02`
- `20260810-191225-agent-1230-script-first-r2-task03`
- `20260810-191236-agent-1230-script-first-r2-task07`
- `20260810-191254-agent-1230-script-first-r3-task02`
- `20260810-191304-agent-1230-script-first-r3-task03`
- `20260810-191320-agent-1230-script-first-r3-task07`
