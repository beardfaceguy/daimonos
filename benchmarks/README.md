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
| `bench-tool-output.sh` | Deterministic API-free A/B for tool-output bounding and microcompaction. |
| `bench-cursor.sh` | Runs the suite through `cursor-agent` (external-agent comparison). |
| `bench-codex.sh` | Runs the suite through `codex exec` via OpenRouter (controlled harness A/B). |
| `analyze.py` | Aggregates one or more result dirs, grouped by model, correctness-gated. |
| `context_compare.py` | Separates call-count and mean-context effects and ranks context components. |
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

## Controlled tool-output benchmark

`bench-tool-output.sh` runs the same scripted agent turn twice with the final
binary:

- **baseline** — output and microcompaction limits set effectively unbounded;
- **candidate** — default 50 KiB output cap, 40k old-result token budget, five
  protected recent results, and 2,000-character old argument threshold.

The scenario deterministically drives oversized native `read_file`,
`list_all_tools` meta, `execute_script`, and remote MCP results; multiple
medium remote results that exceed the intra-turn budget; old `write_file` and
`edit_file` arguments; and one preserved error result. It asserts tool
call/result pairing, recoverability, read-cache invalidation, and analytics
hits before reporting model-visible tokens and wall time.

```sh
./bench-tool-output.sh
```

No provider or API key is used. Results are written under
`benchmarks/results/*-controlled-tool-output*.json`.

## Native context composition diagnostics

`--debug-tokens` generation records use additive schema version 2. In addition
to provider usage, each native agent generation records numeric-only context
composition: system bytes, tool names/descriptions/schemas, user/assistant
text, thinking/provider state, tool-call arguments, successful/error tool
results, and encoded image bytes. Prompt or tool content is never logged.

`extract_tokens.py` aggregates these records into each task summary, including
actual prompt occupancy, context coverage/growth, component exposure, tool-loop
call count, and final-call count. Old logs remain valid and report no context
coverage instead of treating missing measurements as zero.

Compare repeated baseline and candidate run directories with:

```sh
python3 context_compare.py \
  --baseline results/<baseline-r1> results/<baseline-r2> \
  --candidate results/<candidate-r1> results/<candidate-r2>
```

The comparison uses a symmetric two-factor decomposition:

- call-count effect — additional/fewer model requests at the average context;
- mean-context effect — larger/smaller requests at the average call count.

The two effects sum exactly to the total prompt-token difference.

## Optimization lineage policy

Every optimization benchmark must append a stage to
[`results/optimization-lineage.md`](results/optimization-lineage.md) before
shipping. Do not overwrite prior stages.

Record both:

- immediate delta from the directly preceding implementation;
- cumulative delta from the original comparable baseline.

Include per-task regressions even when the aggregate improves. Stages with
different task sets, providers, models, thinking levels, fixture commits, or
correctness gates must use separate scope fingerprints and must not be compared
as one continuous total.

### SQLite benchmark database

The tracked machine-readable source is `benchmarks/optimization-lineage.json`.
Synchronize it and any available raw run directories into the private local
database:

```sh
python3 benchmarks/benchmark_db.py \
  --db ~/.daimonos/benchmarks.db \
  sync \
  --manifest benchmarks/optimization-lineage.json \
  --results-dir benchmarks/results
```

The database is created with mode `0600`; it is local and never committed.

```sh
# List immutable stages
python3 benchmarks/benchmark_db.py --db ~/.daimonos/benchmarks.db list

# Compare matching stages, including every per-task regression
python3 benchmarks/benchmark_db.py \
  --db ~/.daimonos/benchmarks.db compare F0 F1

# Query directly
sqlite3 ~/.daimonos/benchmarks.db
```

`compare` rejects stages with different scope or task-set fingerprints.

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
