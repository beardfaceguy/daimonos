# server-bench

Deterministic, LLM-free benchmark for daimonos.

## Why

The existing `benchmarks/` suite drives an LLM agent through fixed tasks
and measures token cost + wall time. That's useful for end-user-realistic
numbers but its run-to-run variance is dominated by the LLM's strategy
choices, not by daimonos itself. We measured this in May 2026 (Vikunja
#264): post-fix vs pre-fix benchmarks showed a +17.8% cost spike that
turned out to be one bad Starlark syntax retry, not a server regression.

`server-bench` cuts the LLM out entirely. The harness opens a Unix
socket to a single daimonos daemon, sends fixed opcode sequences, and
times every round-trip individually. Variance reflects only daimonos
+ kernel + harness — typically <5% stdev across the wall-time series.

## Quick start

```bash
cargo build --release   # if you haven't already
python3 benchmarks/server-bench/bench.py
```

Default: 4 tasks × 20 replicates, results dropped in
`benchmarks/server-bench/results/<timestamp>/results.json`.

To compare two runs:

```bash
python3 benchmarks/server-bench/compare.py \
    benchmarks/server-bench/results/baseline/ \
    benchmarks/server-bench/results/candidate/
```

## Tasks

Each task is a Python module under `tasks/`. They share one interface:

| Symbol | Type | Purpose |
|--------|------|---------|
| `ID` | `str` | Task id, matches the filename stem |
| `DESCRIPTION` | `str` | Short human label |
| `setup(workspace)` | callable | Build any fixtures the run needs |
| `run_iteration(client, workspace)` | callable | Execute one replicate; returns `[(op_code, elapsed_ns), ...]` |

Built-in tasks:

| Id | What it measures |
|----|------------------|
| `read_100` | Opcode 0 (read) dispatch + JSON framing on a hot read cache |
| `search_many` | Opcode 6 (grep) cost — ripgrep spawn + result formatting |
| `snapshot_cycle` | Opcodes 12/13/26 — async-fs snapshot create/restore/delete |
| `exec_burst` | Opcode 8 (exec) — fork + exec + waitpid + framing |

## Output schema

```json
{
  "binary": "/path/to/target/release/daimonos",
  "replicates": 20,
  "tasks": [
    {
      "task_id": "read_100",
      "description": "...",
      "iterations": 20,
      "timings_ns": [12345, 13456, ...],
      "op_codes": [0, 0, ...],
      "resources": {
        "rss_kb": 18432,
        "fd_count": 12,
        "inotify_watches": 1
      },
      "summary": {
        "count": 2000,
        "min_ns": 11111,
        "max_ns": 222222,
        "mean_ns": 14000,
        "median_ns": 13500,
        "p95_ns": 18000,
        "p99_ns": 25000,
        "stdev_ns": 2500
      }
    }
  ]
}
```

`timings_ns` is the per-call series so downstream tools can compute
distributions however they want (the `summary` block is just a quick
glance).

## Adding a task

1. Create `tasks/<id>.py` with `ID`, `DESCRIPTION`, `setup`, and
   `run_iteration` as above.
2. Add `<id>` to `DEFAULT_TASKS` in `bench.py` if it should run by
   default.

Use the existing tasks as templates — `read_100.py` is the simplest
static-sequence example, `snapshot_cycle.py` is the dynamic example
where each op's input depends on the previous op's output.

## What this doesn't replace

The LLM-driven benchmark in `benchmarks/` is still the right answer for
"will users see a difference?" — its variance is real and reflects
something real (model strategy is part of the end-to-end experience).
`server-bench` is the right answer for "is there a server-side
regression?", where you want to remove the model's confounding effect
entirely.
