#!/usr/bin/env bash
# mini-swe-agent on the stratified 20, then evaluate both prediction sets.
set -u
cd "$(dirname "$0")" || exit

K="$(sed -n 's/^DAIMONOS_AGENT_API_KEY=//p' ~/.config/daimonos/agent.env)"
usage() { curl -s https://openrouter.ai/api/v1/key -H "Authorization: Bearer $K" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['usage'])"; }

IDS_RE="$(paste -sd'|' stratified20.txt)"
DAIMONOS_PREDS="${DAIMONOS_PREDS:-}"
if [[ -z "$DAIMONOS_PREDS" || ! -f "$DAIMONOS_PREDS" ]]; then
  echo "Set DAIMONOS_PREDS to a full-50 daimonos preds.jsonl file." >&2
  exit 2
fi

echo "PHASE_MARK strat_usage_start $(usage)"
export OPENROUTER_API_KEY="$K" MSWEA_COST_TRACKING='ignore_errors'
.venv/bin/mini-extra swebench --subset MariusHobbhahn/swe-bench-verified-mini --split test \
  --filter "$IDS_RE" -m openrouter/anthropic/claude-opus-4.8 \
  -o results/mini-strat20 --workers 1 2>&1 | tail -8
echo "PHASE_MARK mini_strat_done usage=$(usage)"

.venv/bin/python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path "$DAIMONOS_PREDS" \
  --max_workers 4 --run_id eval-daimonos-full50 2>&1 | tail -12
echo "PHASE_MARK eval_daimonos_done"

.venv/bin/python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path results/mini-strat20/preds.json \
  --max_workers 4 --run_id eval-mini-strat20 2>&1 | tail -12
echo "PHASE_MARK eval_mini_done"
echo "PHASE_MARK strat_all_done"
