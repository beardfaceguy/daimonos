# SWE-bench Verified (mini) benchmark for daimonos

Runs **daimonos-as-agent** against [swe-bench-verified-mini](https://huggingface.co/datasets/MariusHobbhahn/swe-bench-verified-mini)
(50 instances, a distribution-matched subset of SWE-bench Verified) and scores
the patches with the official SWE-bench Docker evaluation harness. This gives
a community-comparable claim: resolve rate at X tokens / $Y per task, next to
published numbers such as the token-consumption study in
[arXiv:2604.22750](https://arxiv.org/abs/2604.22750).

Unlike the in-house suite (`../bench-agent.sh`), correctness here is external:
the harness applies each instance's held-out `test_patch` and runs
FAIL_TO_PASS / PASS_TO_PASS tests in a per-instance Docker image.

## One-time setup

```sh
uv venv --python 3.12 .venv
uv pip install --python .venv/bin/python \
  'swebench==5.0.2' 'mini-swe-agent==2.4.6'
.venv/bin/python fetch_dataset.py      # writes instances.jsonl (50 rows)
```

Evaluation additionally needs a working Docker daemon your user can talk to
(`docker info` must succeed without sudo).
After adding yourself to the `docker` group, log out and back in before testing
access from an existing desktop session.

Restoration reference (2026-09-09): uv 0.12.10, CPython 3.12.14,
and SWE-bench 5.0.2. `fetch_dataset.py` verifies the canonical source rows
before atomically replacing `instances.jsonl`:

- source mini-dataset SHA-256:
  `93185a2cce684b2b99592edeb83e16a47806c456fe335931cc92747601584a88`;
- enriched `instances.jsonl` SHA-256:
  `6ce05e6b926c91faecdbb4243014ac85dca471dfad30ccdb69453343b75267c3`.

The tracked fetch step uses SWE-bench's own pinned image-name helper to add the
official evaluator image required by each Docker runner.

## Running

Model/provider/key come from `~/.config/daimonos/agent.env`, exactly like
`bench-agent.sh` (the runner copies it with `APPROVAL_MODE=auto` and
`COMPACTION=off`).

```sh
# Single-instance smoke test FIRST (same cost policy as ../README.md):
.venv/bin/python run_agent.py --docker \
  --instance-ids django__django-11815 --tag smoke

# Full 50-instance run:
.venv/bin/python run_agent.py --docker --tag <label>

# Cursor arm (requires cursor-agent login):
.venv/bin/python run_cursor.py --docker \
  --model claude-opus-4-8-medium --tag <label>

# mini-swe-agent arm (expects OPENROUTER_API_KEY in the environment):
OPENROUTER_API_KEY=... .venv/bin/mini-extra swebench \
  --subset MariusHobbhahn/swe-bench-verified-mini --split test \
  --model openrouter/anthropic/claude-opus-4.8 \
  --environment-class docker --workers 1 --output results/mini-<label>
```

Each run writes `results/<run-id>/` with per-instance token/cost JSONs
(same schema as the in-house suite — `../analyze.py results/` works),
`.patch` files, raw transcripts, and `preds.jsonl`.
mini-swe-agent writes trajectories plus `preds.json`; normalize each trajectory
through `extract_mini.py` before cross-harness analysis. The normalizer sums
OpenRouter's per-generation `usage.cost`, the same accounting source Daimonos
uses, and separates fresh/cache-write/cache-read prompt tokens; LiteLLM's
aggregate is retained only as a consistency check. Cursor uses its own backend,
so token/correctness comparisons are available but USD cost parity is not.

Current default-harness result:
[`2026-09-10-swebench-openrouter-three-repetitions.md`](../results/2026-09-10-swebench-openrouter-three-repetitions.md).

Delete incomplete smoke directories created before dataset enrichment before
treating `results/` as a baseline; a valid run contains `preds.jsonl`, a
per-instance summary, token log, raw transcript, and non-empty patch.

Repo checkouts are cached as bare clones under `repos/` (first run downloads
each project once; django/astropy/sympy etc. total a few GB).

## Evaluating

```sh
.venv/bin/python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path results/<run-id>/preds.jsonl \
  --max_workers 2 --run_id <run-id>
```

Zero-LLM-cost plumbing check (evaluates the dataset's own gold patches):

```sh
.venv/bin/python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path gold \
  --instance_ids django__django-11815 \
  --max_workers 1 --run_id gold-smoke
```

The harness writes `<model>.<run_id>.json` with `resolved_ids` /
`unresolved_ids`; join those against the runner's per-instance JSONs to get
correctness-gated token and cost aggregates.

## Cost note

Same policy as `../README.md`: the OpenRouter account has auto top-up and no
hard cap. Always run the single-instance smoke test and check its cost before
launching the full 50. SWE-bench instances are far heavier than the in-house
tasks — expect hundreds of thousands to millions of tokens per instance
(see arXiv:2604.22750).
