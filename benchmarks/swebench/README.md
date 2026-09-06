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
uv pip install --python .venv/bin/python \
  swebench==5.0.2 mini-swe-agent==2.4.6 datasets==5.0.1
.venv/bin/python fetch_dataset.py      # writes instances.jsonl (50 rows)
sha256sum instances.jsonl
# expected: ea5be657c14d3377657de79fe175218f82d02670dc1ba8a14d3825687c3ce9e3
```

Evaluation additionally needs a working Docker daemon your user can talk to
(`docker info` must succeed without sudo).

The recorded dataset revision is
`MariusHobbhahn/swe-bench-verified-mini@b316c349947c29963fce3f4a65967c9807a4b673`
(Hugging Face dataset fingerprint `6a0bdaa3eed285fa`). If the dataset's
`main` branch moves and `fetch_dataset.py` no longer produces the expected
hash, regenerate the historical input explicitly before comparing a new run
with the September 2026 results:

```sh
.venv/bin/python - <<'PY'
import json
from datasets import load_dataset

revision = "b316c349947c29963fce3f4a65967c9807a4b673"
rows = load_dataset(
    "MariusHobbhahn/swe-bench-verified-mini",
    revision=revision,
    split="test",
)
with open("instances.jsonl", "w") as output:
    for row in rows:
        output.write(json.dumps(row) + "\n")
PY
sha256sum instances.jsonl
```

Build the runner binary from the commit being measured. Host-mode runs use the
normal release binary; Docker runs require the statically linked musl binary:

```sh
cargo build --release
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Running

Model/provider/key come from `~/.config/daimonos/agent.env`, exactly like
`bench-agent.sh` (the runner copies it with `APPROVAL_MODE=auto` and
`COMPACTION=off`).

```sh
# Single-instance smoke test FIRST (same cost policy as ../README.md):
.venv/bin/python run_agent.py --filter astropy__astropy-14309 --tag smoke

# Full 50-instance run:
.venv/bin/python run_agent.py --tag <label>

# Full 50 inside the official per-instance test containers:
.venv/bin/python run_agent.py --docker --tag full50

# Cursor comparison (requires cursor-agent login):
.venv/bin/python run_cursor.py --docker \
  --model claude-opus-4-8-high --tag cursor-full50
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

## Regenerating local artifacts

The following generated paths are intentionally ignored by git. Recreate them
from a clean checkout in this order:

| Artifact | Regeneration |
|---|---|
| `.venv/` | Run the pinned `uv venv` / `uv pip install` commands above. |
| `instances.jsonl` | Run `fetch_dataset.py`, then verify its SHA-256 above. |
| `repos/` | Created lazily by either runner; delete it to force fresh clones. |
| `worktrees/` | Temporary runner output; normally removed automatically. |
| `logs/` | Created by the official SWE-bench evaluator. |
| `results/<run-id>/` | Run `run_agent.py` or `run_cursor.py`; each directory contains per-instance JSON, patches, transcripts, and predictions. |
| `results/mini-*` | Run `mini-extra swebench` as shown in `run50.sh` or `run_strat20.sh`. |
| top-level `*.json` | Run the official evaluator against the corresponding `preds.jsonl`/`preds.json`; its `<model>.<run_id>.json` report is written here. |

To rerun the full daimonos/mini-swe-agent comparison and both official
evaluations:

```sh
./run50.sh
```

To rerun the committed stratified-20 comparison, first produce a daimonos
full-50 prediction file, then pass it explicitly:

```sh
DAIMONOS_PREDS=results/<daimonos-run-id>/preds.jsonl ./run_strat20.sh
```

The committed files `results/3way-summary.md` and
`results/strat20-analysis.txt` are the durable human-readable records. Rebuild
them by joining each runner's per-instance JSON summaries with the evaluator's
`resolved_ids` and `unresolved_ids`; keep failed instances visible rather than
reporting aggregate totals alone.

Before publishing a new comparison, record:

```sh
git rev-parse HEAD
sha256sum ../../target/release/daimonos \
  ../../target/x86_64-unknown-linux-musl/release/daimonos
uv pip freeze --python .venv/bin/python
```

Also retain the exact provider/model slug, thinking level, selected instance
IDs, runner command, run directories, and evaluator run IDs. Dataset,
checkout, and evaluator artifacts are mechanically reproducible from these
instructions. Hosted LLM trajectories and their patches are only rerunnable,
not byte-for-byte reproducible, because model implementations and sampling can
change behind a stable API slug.

## Cost note

Same policy as `../README.md`: the OpenRouter account has auto top-up and no
hard cap. Always run the single-instance smoke test and check its cost before
launching the full 50. SWE-bench instances are far heavier than the in-house
tasks — expect hundreds of thousands to millions of tokens per instance
(see arXiv:2604.22750).
