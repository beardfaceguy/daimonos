# Daimonos agent-mode benchmark

Measures how **daimonos itself, acting as the agent**, performs on a fixed suite
of coding tasks: tokens consumed, provider cost, wall time, and a
machine-checkable correctness verdict per task.

This is *daimonos-as-agent* (`daimonos agent`, using its own native tools) — not
daimonos-as-an-MCP-tool-server for some other agent. There is one runner and one
analyzer; no runtime/arm matrix, no cursor/Claude comparison, no model list to
keep current.

## Files

| File | Role |
|---|---|
| `bench-agent.sh` | The runner. Runs the task suite through `daimonos agent`. |
| `bench-cursor.sh` | Runs the suite through `cursor-agent` (external-agent comparison). |
| `bench-codex.sh` | Runs the suite through `codex exec` via OpenRouter (controlled harness A/B). |
| `analyze.py` | Aggregates one or more result dirs, grouped by model, correctness-gated. |
| `tasks/*.json` | The task suite (prompt + machine-checkable `checks`). |
| `workspace/` | A git repo the agent operates in; reset (`git checkout` + `clean`) before each task. |
| `check_task.py` | Correctness gate: runs each task's `checks` against the response + workspace. |
| `extract_tokens.py` | Normalizes the `--debug-tokens` log delta into a per-task summary JSON. |
| `summarize.py` | Prints the end-of-run per-task summary table (shared by the runners). |
| `results/` | One dir per run, one JSON per task. |
| `server-bench/` | Separate deterministic opcode/transport micro-bench. Unrelated to this. |

## Model, provider, and API key — one source of truth

All of these come from the **agent env file**, `~/.config/daimonos/agent.env`
(override with `DAIMONOS_AGENT_ENV`). Whatever `DAIMONOS_AGENT_MODEL`,
`DAIMONOS_AGENT_PROVIDER`, `DAIMONOS_AGENT_BASE_URL`, and `DAIMONOS_AGENT_API_KEY`
that file holds is exactly what runs and what gets billed — the same config the
interactive ACP session uses. There is no `models.json` and no baked-in
`--model` override, so the file cannot be silently overridden.

The runner makes a temporary copy of the agent env with `APPROVAL_MODE=auto` (so
it never blocks on a permission prompt mid-suite) and `COMPACTION=off` (so a run
needs no resolved context window). The key/provider/model are inherited
unchanged; the temp file is deleted on exit.

To benchmark a different model, change `DAIMONOS_AGENT_MODEL` in `agent.env`
(or set `MODEL=<slug>` for a one-off) and run again with a distinct `BENCH_TAG`.

## Quick start

```sh
# Single-task smoke test FIRST — confirm plumbing, output, and billing before
# spending on the full suite (the OpenRouter account has no hard spend cap):
./bench-agent.sh 01

# Full suite:
./bench-agent.sh

# Analyze everything under results/ (or filter by tag substring):
python3 analyze.py results/
python3 analyze.py results/ sol
```

## Environment variables

| Var | Default | Meaning |
|---|---|---|
| `BENCH_TAG` | *(none)* | Label folded into the results dir name. Use one per model so `analyze.py` can group runs. |
| `MODEL` | *(from agent.env)* | Override the model for this run only. Normally unset. |
| `DAIMONOS_BIN` | `../target/release/daimonos` | Which binary to exercise. |
| `DAIMONOS_AGENT_ENV` | `~/.config/daimonos/agent.env` | Source agent env file. |
| `BENCH_BUILD` | `0` | `1` = `cargo build --release` first, to guarantee current code. |
| `BENCH_TASK_TIMEOUT` | `600` | Per-task wall-clock cap in seconds (kills a stuck task so it can't burn credits unbounded). |

## Tasks and correctness

Each `tasks/NN-*.json` has a `prompt` and a `checks` array. Check shapes:

- `{"type":"response","all":["pat", ...]}` — every (case-insensitive) regex must match the response.
- `{"type":"response","any":["pat", ...],"min":2}` — at least `min` of them match.
- `{"type":"workspace","command":"sh command"}` — command exits 0 in the workspace (filesystem ground truth).

`check_task.py` stamps `checks_passed`, `checks_total`, and `correct`
(true / false / null-if-no-checks) into each task summary. `analyze.py` excludes
`correct == false` and errored runs from token/cost aggregates so a model can't
look cheap by failing.

## Cost note

`analyze.py` reports the real `cost_usd` daimonos logs from the provider; no
prices are hardcoded, so it stays correct as models and pricing change. The
OpenRouter account this bills has auto top-up and no hard cap — always run the
single-task smoke test before a full suite, and check the projected cost.
