# #1230 Phase 2 — read → transform → write intervention

Date: 2026-08-14 · Commit: `50b0c86425c6f3f51f3aef54a34866d0f5a5b14c`
· Provider: Anthropic · Model: `claude-opus-4-8` · Thinking: medium · Prompt caching: OFF

## Design

Two binaries built from the same commit, differing **only** in
`prompts/agent_system.md`:

- **base** — the prompt as it stands
- **rtw** — adds one subsection: *"You do not need to read a file into your
  context to change it"*, with the verified-optimal task-03 script as a concrete
  example

Nothing else differs. In particular the tool catalog is identical across arms,
so the tool-schema confound that invalidated the PR #155 experiment does not
apply here: neither arm gets a mechanical request-size discount.

Task 02 dropped (already at the 2-call floor, contributes only noise — PR #159).
Task 03 gets n=3 (deterministic), task 07 n=5 (historically noisy).

## Results

| task | arm | calls | mean | adoption | $/run | correct |
|---|---|---|---|---|---|---|
| 03 | base | [5, 5, 5] | 5.00 | 0/3 | $0.3499 | 3/3 |
| 03 | rtw | [2, 2, 3] | **2.33** | **3/3** | $0.1578 | 3/3 |
| 07 | base | [6, 6, 6, 6, 6] | 6.00 | 1/5 | $0.4262 | 5/5 |
| 07 | rtw | [3, 4, 4, 4, 5] | **4.00** | **5/5** | $0.3055 | 5/5 |

- Task 03: **−53.3% calls**, −54.9% cost
- Task 07: **−33.3% calls**, −28.3% cost
- Correctness **16/16**, no regression in either arm
- Total spend: $5.18 over 16 runs

Across the deliberately uneven 3+5 task repetitions, the run-weighted mean
moved from 5.625 to 3.375 calls (**−40.0%**), 76,282.9 to 46,241.8 tokens
(**−39.4%**), and $0.3976 to $0.2501 (**−37.1%**) per run. Batch adoption moved
from 1/8 to 8/8 runs.

### Retained per-task regressions

Task 07's mean output tokens increased **47.9%** (893.0 → 1,320.4) and mean
wall time increased **17.3%** (16.2s → 19.0s), despite its total-token, call,
and cost improvements. Task 03 had no upward metric movement. These regressions
remain part of the result; the aggregate cost/call win does not erase them.

## Why small n is sufficient here

The usual objection — n=3 cannot resolve a 22% effect when a
configuration-identical control swings 15% (PR #155) — does not apply, because
**the base arm has zero variance**: 5,5,5 and 6,6,6,6,6. The per-arm ranges are
**disjoint** on both tasks. There is no distribution overlap to resolve.

The base measurement also reproduces PR #159's independent observation that task
03 sits at exactly 5 calls deterministically, which validates the harness setup
rather than resting on this run alone.

Task 03's rtw arm reached **2 calls** twice — the proven floor (one batched
script plus one final turn). The intervention attains the known optimum, not
merely an improvement.

## Instrument validation

`max_script_ops: 0` on the first baseline runs was verified to be a *true
reading*, not a dead counter, by cross-checking `analytics.db` per session:

- task 03 base: `search → read_file → edit_file → cargo` — 4 tool calls, **0**
  `script:*` sub-calls
- task 07 base: `snapshot ×2, search ×2, read_file, edit_file` — 6 tool calls,
  **0** `script:*` sub-calls

This matters because the metric has twice measured the wrong thing on this task.
Note `analytics.db` is shared with any live daimonos MCP session on the machine,
so per-`session_id` grouping is required; an aggregate query over recent rows is
contaminated by the operator's own tool calls.

The task-03 base trace is exactly the read-then-edit pattern the intervention
targets, so the experiment acts on the observed behaviour rather than a
hypothesised one.

## Caveats

- One model (`claude-opus-4-8`), caching off, two tasks. Not shown to generalise
  to other models or to the eight tasks at or near the call floor.
- Task 07 base showed 1/5 adoption: one base run *did* emit a qualifying script
  and still took 6 calls, so adoption alone does not determine call count.
- Absolute costs are cache-off; the ratio is what transfers.

## Reproduce

```sh
cd benchmarks
./run-1230-phase2.sh 03 3     # both arms, n=3
./run-1230-phase2.sh 07 5     # both arms, n=5
python3 /tmp/analyze_1230.py  # or re-derive from results/*1230p2*/**.json
```

The first accepted task-03 baseline run is the directory tagged
`20260814-143911-agent-1230p2-smoke`; it was retained as base repetition 1
because it used the same base binary/config and passed the same correctness
gate. The machine-readable `R0`/`R1` stages in
`benchmarks/optimization-lineage.json` enumerate all 16 accepted raw run
directories, binary hashes, fixture commit, task-set fingerprint, and
immediate/cumulative deltas.
