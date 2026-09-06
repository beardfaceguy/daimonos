#!/usr/bin/env bash
# Full 50-instance daimonos vs mini-swe-agent comparison, in-container.
set -u
cd "$(dirname "$0")" || exit

K="$(sed -n 's/^DAIMONOS_AGENT_API_KEY=//p' ~/.config/daimonos/agent.env)"
usage() { curl -s https://openrouter.ai/api/v1/key -H "Authorization: Bearer $K" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['usage'])"; }

echo "PHASE_MARK usage_start $(usage)"

.venv/bin/python run_agent.py --docker --tag full50
DAIMONOS_RC=$?
echo "PHASE_MARK daimonos_done rc=$DAIMONOS_RC usage=$(usage)"

export OPENROUTER_API_KEY="$K" MSWEA_COST_TRACKING='ignore_errors'
.venv/bin/mini-extra swebench --subset MariusHobbhahn/swe-bench-verified-mini --split test \
  -m openrouter/anthropic/claude-opus-4.8 -o results/mini-full50 --workers 1 2>&1 | tail -8
echo "PHASE_MARK mini_done usage=$(usage)"

shopt -s nullglob
DAIMONOS_RUNS=(results/*-swebench-docker-full50)
if ((${#DAIMONOS_RUNS[@]} == 0)); then
  echo "No daimonos full-50 result directory found." >&2
  exit 2
fi
DAIMONOS_DIR="${DAIMONOS_RUNS[${#DAIMONOS_RUNS[@]}-1]}"
.venv/bin/python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path "$DAIMONOS_DIR/preds.jsonl" \
  --max_workers 4 --run_id eval-daimonos-full50 2>&1 | tail -15
echo "PHASE_MARK eval_daimonos_done"

.venv/bin/python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path results/mini-full50/preds.json \
  --max_workers 4 --run_id eval-mini-full50 2>&1 | tail -15
echo "PHASE_MARK eval_mini_done"
echo "PHASE_MARK all50_done"
