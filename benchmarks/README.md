# Daimonos Token Usage Benchmark

Comparative benchmark measuring token consumption when an AI agent performs
identical coding tasks using Cursor's built-in tools vs. daimonos MCP tools.

**Linear issue:** CLA-167

## Quick start

```bash
# Full gated run (build, MCP setup, smoke gate, then baseline /
# baseline-terse / daimonos / daimonos-terse at BENCH_RUNS each, then analysis):
./run-all-arms.sh gated

# Or run a single arm (baseline | baseline-terse | daimonos | daimonos-terse):
./setup-mcp.sh
BENCH_RUNS=4 ./run-benchmark.sh daimonos

# Analyze a tag across arms (means with min–max spread, correctness-gated):
python3 analyze-results.py results/ gated
```

`baseline-terse` is the prompt-control arm: built-in tools plus the same
terse-style directive the daimonos arm carries. `daimonos vs baseline-terse`
isolates the tools-only effect; `daimonos vs baseline` is the deployed effect.

`daimonos-terse` is the same as the daimonos arm but runs the MCP server at
terse verbosity (`DAIMONOS_MCP_VERBOSITY=terse`), so the prefix-diet levers
(#936: kgl context-gating, terse tool descriptions) actually apply.
`daimonos-terse vs baseline-terse` is the tools-only effect *with* those levers;
`daimonos-terse vs daimonos` is the lever delta itself (#945).

## Runtime & cost expectations (measured)

Measured 2026-07-07 on the full four-arm gated suite (`./run-all-arms.sh`,
`BENCH_RUNS=4`, model `opus`, CLI 2.1.143) — 168 task-runs total (baseline and
baseline-terse run 10 tasks each, daimonos and daimonos-terse 11 each, ×4 runs):

| Checkpoint | Elapsed (arms) | Progress |
|---|--:|--:|
| T0 | ~+3 min  | 7.1%  (12/168) |
| T1 | ~+13 min | 36.9% (62/168) |
| T2 | ~+23 min | 67.3% (113/168) |
| done | ~+35.5 min | 100% (168/168) |

- **Throughput ≈ 5 task-runs/min, roughly linear across all arms.** Tool-op
  tasks are uniformly short; daimonos's MCP overhead shows up in cost/turns
  per task, not wall-clock throughput — so the "daimonos arms are slower"
  intuition does *not* hold for total run time here.
- **Rule of thumb: ~0.2 min per task-run, ~9 min per arm at `BENCH_RUNS=4`.**
  Scales linearly with `BENCH_RUNS` and the number of arms.
- **Arms wall ≈ 35 min (measured 35.5); end-to-end ≈ 37–40 min** for a 4-arm
  `BENCH_RUNS=4` run. Add **~3–5 min for a cold release build** (skip if
  `target/` is warm); setup + smoke gate is ~1 min.
- **Cost ≈ $5–6** for the 4-arm run (the earlier 3-arm gated run was ~$4.20).

Plan a full run at **~40 minutes**, not the hour-plus it might feel like — an
early ad-hoc estimate of ~1.5h was ~2.7× too high because these tasks are short.

**Latest results and methodology: [results/2026-07-06-gated-three-arm.md](results/2026-07-06-gated-three-arm.md)**
(the pre-2026-07 numbers below and any “~27% savings” figures are superseded).

## Runtime comparison (daimonos-agent vs Claude CLI vs Cursor CLI)

**Different axis from the benchmark above.** `run-benchmark.sh` runs the *same*
Claude CLI for every arm and only toggles the daimonos MCP on/off — it isolates
the *tools*. `run-runtime-benchmark.sh` instead compares three whole **agent
runtimes**, each using its *own native tools*, on identical tasks:

| Runtime | Command | Tools |
|---|---|---|
| `daimonos` | `daimonos agent … --agent-env <bench-env>` | native daimonos tools |
| `claude` | `claude -p --output-format stream-json` | Claude Code built-in tools |
| `cursor` | `cursor-agent -p --output-format stream-json --force` | cursor-agent built-in tools |

### Quick start

```bash
# One runtime at a time (writes results/<ts>-<runtime>-<model>[-tag]/):
BENCH_TAG=run1 ./run-runtime-benchmark.sh daimonos
BENCH_TAG=run1 ./run-runtime-benchmark.sh claude
BENCH_TAG=run1 ./run-runtime-benchmark.sh cursor

# Normalized, correctness-gated token report across whatever runtimes are present:
python3 analyze-runtimes.py results run1

# Cursor emits tokens inline but NOT cost; after the Cursor admin usage report
# populates, join dollar cost by time window (download the CSV from the admin page):
python3 cursor-attribute.py results/<cursor-run-dir> ~/Downloads/team-usage-events-*.csv
```

CSV attribution requires each row's `Model` to exactly match the task
summary's `model_slug`; in-window events for other models are counted in
`cursor_csv_ignored_model_rows` and excluded. A summary without `model_slug`
fails closed and leaves cost unset.

### Models

Slugs live in `models.json` as a canonical→per-runtime map, so adding Opus/fable
is a one-line edit. Pick one with `BENCH_CANON_MODEL` (default `sonnet`, the
non-thinking baseline). A `null` slug means that runtime skips that model (e.g.
fable is not in daimonos's OpenRouter allowlist).

```bash
BENCH_CANON_MODEL=opus ./run-runtime-benchmark.sh claude
```

### Normalized metric schema

Every runtime is collapsed to one schema (`extract-tokens.js`):
`input` (fresh/non-cached) · `cache_write` · `cache_read` · `output` ·
`total_tokens` (= sum of the four, matching Cursor's admin "Total Tokens") · `cost_usd`.

Lead with **token counts** for efficiency. **Costs are not provider-neutral**
(daimonos→OpenRouter, claude→Anthropic, cursor→Cursor) and daimonos via
OpenRouter often reports `0` — tokens are the honest cross-runtime metric.

### Runtime notes

- **daimonos runs headless** via a generated bench env
  (`~/.config/daimonos/agent.bench.env`, kept out of the repo — it has the API
  key): a copy of your `agent.env` with `APPROVAL_MODE=auto` and
  `COMPACTION=off`. Per-task token usage is read from the `--debug-tokens` log
  delta (only the new lines for that run).
- **Cursor `tool_calls` is reported as 0** — cursor-agent's stream-json does not
  expose `tool_use` blocks the way Claude's does. Token counts are unaffected.
- **daimonos reports `llm_calls` instead of `tool_calls`** — its token log
  records LLM round-trips, not tool invocations, so `tool_calls` is `null`.
- **The cursor arm moves `workspace/.cursor/mcp.json` aside** for the duration
  (restored on exit) — it registers the daimonos MCP server for the tool-config
  benchmark and would contaminate a native-tools-only arm.
- Task 07 (snapshot) is daimonos-tool-specific and is skipped by all three arms.

## Structure

```
benchmarks/
├── README.md                  # This file
├── run-benchmark.sh           # Tool-config benchmark (Claude CLI, MCP on/off)
├── run-runtime-benchmark.sh   # Runtime benchmark (daimonos/claude/cursor, native tools)
├── models.json                # Canonical→per-runtime model slug map
├── extract-tokens.js          # Normalizes per-runtime usage into one schema
├── analyze-runtimes.py        # 3-arm normalized, correctness-gated token report
├── cursor-attribute.py        # Joins Cursor admin CSV cost by time window
├── setup-mcp.sh               # Configures .cursor/mcp.json for daimonos mode
├── analyze-results.py         # Compares results across runs
├── compare-models.py          # Cross-model comparison report
├── remote/                    # AWS remote benchmark orchestration
│   ├── run-remote-benchmark.sh    # Master orchestrator (launch, provision, run, collect)
│   ├── provision-ubuntu.sh        # Provisions Ubuntu instance
│   ├── provision-daimonos.sh      # Provisions daimonos distro instance
│   └── collect-results.sh         # Standalone result collector
├── workspace/                 # Target codebase (small Rust inventory app)
│   ├── Cargo.toml
│   ├── README.md
│   ├── .gitignore
│   ├── .cursor/mcp.json       # Auto-generated by setup-mcp.sh
│   ├── data/stock.csv
│   └── src/
│       ├── main.rs
│       ├── config.rs
│       ├── inventory.rs
│       └── report.rs
├── tasks/                     # Task definitions (JSON)
│   ├── 01-read-understand.json
│   ├── 02-search-usages.json
│   ├── 03-edit-rename.json
│   ├── 04-explore-architecture.json
│   ├── 05-execute-tests.json
│   ├── 06-git-status.json
│   └── 07-snapshot-rollback.json    # daimonos-only
└── results/                   # Output directory (gitignored)
    └── <timestamp>-<mode>/
        ├── <task-id>.json         # Parsed metrics
        └── <task-id>.raw.jsonl    # Raw agent stream output
```

## Tasks

| # | Task | Category | Cursor | Daimonos |
|---|------|----------|--------|----------|
| 01 | Read & understand a file | file_read | yes | yes |
| 02 | Search for function usages | search | yes | yes |
| 03 | Rename a variable across a file | edit | yes | yes |
| 04 | Explore directory structure | multi_file | yes | yes |
| 05 | Run tests and interpret results | execute | yes | yes |
| 06 | Check git status and history | git | yes | yes |
| 07 | Snapshot, modify, and rollback | snapshot | no | yes |
| 08 | Run cargo test/build via shell | exec_filter | yes | yes |
| 09 | Run git commands via shell | exec_filter | yes | yes |
| 10 | Run build + lint via shell | exec_filter | yes | yes |
| 11 | Multi-command shell workflow | exec_filter | yes | yes |

Task 07 is daimonos-only since Cursor has no native snapshot/rollback capability.

Tasks 08-11 are `exec_filter` tasks that exercise the exec output filtering
pipeline (L1: plugin redirect, L2: semantic output filters). In daimonos mode,
the system prompt instructs the agent to use `exec()` instead of native tools,
so commands like `cargo test` flow through the exec pipeline where L1 redirects
them to the cargo plugin for structured JSON, and L2 filters output for
unrecognized commands. In baseline mode, the agent runs raw shell commands.

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BENCH_MODEL` | `claude-4.6-opus-high` | Model slug for the agent CLI |
| `AGENT_BIN` | `~/.local/bin/agent` | Path to the Cursor agent CLI |
| `DAIMONOS_BIN` | `../target/release/daimonos` | Path to the daimonos binary |

## Running a single task

```bash
./run-benchmark.sh cursor 03   # runs only task 03-edit-rename in cursor mode
./run-benchmark.sh daimonos 01 # runs only task 01-read-understand in daimonos mode
```

## Metrics captured

Per task:
- `input_tokens` — tokens sent to the model
- `output_tokens` — tokens generated by the model
- `cache_read_tokens` — tokens served from cache
- `cache_write_tokens` — tokens written to cache
- `total_tokens` — input + output
- `tool_calls` — number of tool invocations
- `wall_ms` — wall-clock time
- `success` — whether the agent completed without error

## Interpreting results

Run `python3 analyze-results.py results/` after completing both a cursor and
daimonos run. The script produces a side-by-side comparison table showing per-task
and aggregate token usage, with delta and percentage calculations.

Key things to look for:
- **Token savings**: daimonos's compact structured responses should reduce output
  tokens compared to Cursor's verbose tool responses
- **Tool call count**: daimonos may need fewer round-trips due to batched/structured
  responses
- **Snapshot capability**: task 07 demonstrates a workflow that's impossible with
  Cursor's native tools

## Remote benchmarking (AWS)

Run identical benchmarks on two AWS instances to compare tool efficiency on the
same hardware without network latency as a variable. One runs Ubuntu 24.04
(baseline), one runs the daimonos distro with Node.js.

**Linear issue:** CLA-220

### Prerequisites

- Daimonos distro AMI deployed (must include Node.js — see `distro/` build)
- AWS credentials configured (`AWS_PROFILE` or default)
- EC2 key pair for SSH access

### Quick start

```bash
# Build the modified distro (includes Node.js + bench user)
cd distro && ./build-buildroot.sh && ./deploy-aws.sh
# Note the AMI ID from the output

# Run the remote benchmark
cd benchmarks/remote
DAIMONOS_AMI=ami-xxx ./run-remote-benchmark.sh
```

### Options

```bash
# Keep instances alive for debugging
DAIMONOS_AMI=ami-xxx ./run-remote-benchmark.sh --keep

# Run a single task
DAIMONOS_AMI=ami-xxx ./run-remote-benchmark.sh --task 03

# Skip provisioning (re-run on already-provisioned instances)
DAIMONOS_AMI=ami-xxx ./run-remote-benchmark.sh --skip-provision

# Collect results from running instances if orchestrator was interrupted
./collect-results.sh <ubuntu-ip> <daimonos-ip>
```

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DAIMONOS_AMI` | (required) | AMI ID for the daimonos distro |
| `UBUNTU_AMI` | auto-detect | AMI ID for Ubuntu 24.04 |
| `AWS_PROFILE` | `experimental-admin` | AWS CLI profile |
| `AWS_REGION` | `us-east-1` | AWS region |
| `INSTANCE_TYPE` | `t3.medium` | EC2 instance type |
| `KEY_NAME` | auto-detect | EC2 key pair name |
| `SSH_KEY` | `~/.ssh/id_ed25519` | Path to SSH private key |
| `BENCH_MODEL` | `opus` | Model for Claude CLI |
| `BENCH_TAG` | `remote` | Tag for run naming |
