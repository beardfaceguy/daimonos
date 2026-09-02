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
uv venv .venv
uv pip install --python .venv/bin/python swebench
.venv/bin/python fetch_dataset.py      # writes instances.jsonl (50 rows)
```

Evaluation additionally needs a working Docker daemon your user can talk to
(`docker info` must succeed without sudo).

## Running

Model/provider/key come from `~/.config/daimonos/agent.env`, exactly like
`bench-agent.sh` (the runner copies it with `APPROVAL_MODE=auto` and
`COMPACTION=off`).

```sh
# Single-instance smoke test FIRST (same cost policy as ../README.md):
.venv/bin/python run_agent.py --filter astropy__astropy-14309 --tag smoke

# Full 50-instance run:
.venv/bin/python run_agent.py --tag <label>
```

Each run writes `results/<run-id>/` with per-instance token/cost JSONs
(same schema as the in-house suite — `../analyze.py results/` works),
`.patch` files, raw transcripts, and `preds.jsonl`.

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
  --instance_ids astropy__astropy-14309 \
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
