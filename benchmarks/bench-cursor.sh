#!/bin/sh
# bench-cursor.sh — the cursor-agent side of the harness comparison.
#
# Runs the same task suite in `benchmarks/tasks/` through `cursor-agent` in
# headless mode, records per-task tokens / wall time and the SAME
# machine-checkable correctness verdict as `bench-agent.sh`, and writes
# per-task summary JSONs in the identical schema so a single `analyze.py` run
# can compare cursor vs daimonos side by side.
#
# This is the deliberate counterpart to `bench-agent.sh` (daimonos-as-agent).
# It reuses `extract_tokens.py` (cursor branch) and `check_task.py`
# (stream-json format) unchanged; only the runner differs.
#
# BILLING PARITY IS IMPOSSIBLE BY CONSTRUCTION: cursor-agent bills Cursor's own
# backend account, NOT the OpenRouter `daimonos-testing` account daimonos uses.
# Only model + reasoning-effort parity is achievable (gpt-5.6-sol at `high`).
# cursor-agent does not emit per-run cost, so `cost_usd` is null in its
# summaries (analyze.py shows n/a) — do not read a cursor-vs-daimonos cost
# number out of this; the comparison is tokens + correctness at matched effort.
#
# Usage:
#   ./bench-cursor.sh [task-id-prefix]
#
#   task-id-prefix   Optional: run only tasks whose id starts with this
#                    (e.g. "01" for a single-task smoke test). Omit to run all.
#
# Environment variables:
#   CURSOR_MODEL   cursor-agent model slug (default: see DEFAULT_CURSOR_MODEL
#                  below). The default is the match point for daimonos
#                  gpt-5.6-sol at DAIMONOS_AGENT_THINKING=high; cursor only
#                  offers sol at -high/-xhigh, so high is the A/B.
#   BENCH_TAG      Extra label folded into the results dir name (e.g. sol-high-r1).
#   CURSOR_BIN     Path to the cursor-agent binary (default: from PATH, else
#                  ~/.local/bin/cursor-agent).
#   BENCH_TASK_TIMEOUT  Per-task wall-clock cap in seconds (default: 600), same
#                  guardrail as bench-agent.sh: a stuck task must not run away.
#
# Portability: POSIX sh syntax, but requires GNU coreutils `timeout` (for the
# `--kill-after` per-task cap) and `node` on PATH — same host assumptions as
# bench-agent.sh. Intended to run on the Linux benchmark host, not BusyBox.
set -eu

# Single source of the default cursor model slug (referenced in the header).
DEFAULT_CURSOR_MODEL="gpt-5.6-sol-high"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"

# Resolve the cursor-agent binary: explicit CURSOR_BIN, then PATH, then the
# documented default install location.
if [ -n "${CURSOR_BIN:-}" ]; then
  :
elif command -v cursor-agent >/dev/null 2>&1; then
  CURSOR_BIN="$(command -v cursor-agent)"
else
  CURSOR_BIN="$HOME/.local/bin/cursor-agent"
fi

MODEL="${CURSOR_MODEL:-$DEFAULT_CURSOR_MODEL}"
TASK_FILTER="${1:-}"
RUN_TAG="${BENCH_TAG:-}"

[ -x "$CURSOR_BIN" ] || { echo "Error: cursor-agent not found/executable at $CURSOR_BIN"; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "Error: python3 is required (token extraction + correctness checks)"; exit 1; }
command -v timeout >/dev/null 2>&1 || { echo "Error: GNU coreutils 'timeout' is required for the per-task wall-clock cap"; exit 1; }

RUN_ID="$(date +%Y%m%d-%H%M%S)-cursor${RUN_TAG:+-$RUN_TAG}"
RUN_DIR="$RESULTS_DIR/$RUN_ID"
mkdir -p "$RUN_DIR"

echo "=== cursor-agent benchmark ==="
echo "Model:    $MODEL   (cursor-agent)"
echo "Binary:   $CURSOR_BIN"
echo "Run dir:  $RUN_DIR"
[ -n "$TASK_FILTER" ] && echo "Filter:   tasks matching '$TASK_FILTER'*"
echo "NOTE: cursor bills Cursor's own backend, not OpenRouter — token/effort"
echo "      parity only, no cost parity with daimonos runs."
echo

json_field() {
  python3 -c "import json,sys;d=json.load(open(sys.argv[1]));sys.stdout.write(str(d.get(sys.argv[2]) or ''))" "$1" "$2"
}

reset_workspace() {
  cd "$WORKSPACE"
  git checkout -- . 2>/dev/null || true
  git clean -fd 2>/dev/null || true
}

run_task() {
  task_file="$1"
  task_id="$(json_field "$task_file" id)"
  task_name="$(json_field "$task_file" name)"
  applies_to="$(python3 -c "import json,sys;d=json.load(open(sys.argv[1]));sys.stdout.write(','.join(d.get('applies_to') or []))" "$task_file")"
  prompt="$(json_field "$task_file" prompt)"

  if [ -n "$TASK_FILTER" ]; then
    case "$task_id" in "$TASK_FILTER"*) ;; *) return 0 ;; esac
  fi

  # Only run tasks that declare they apply to cursor.
  case ",$applies_to," in
    *",cursor,"*) ;;
    *) echo "  SKIP $task_id ($task_name) — not applicable to cursor"; return 0 ;;
  esac

  echo "  RUN  $task_id: $task_name"
  reset_workspace

  out_file="$RUN_DIR/${task_id}.json"
  raw_file="$RUN_DIR/${task_id}.raw.jsonl"
  err_file="$RUN_DIR/${task_id}.stderr.log"

  start_s="$(date +%s)"
  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  rc=0
  cd "$WORKSPACE"
  # Per-task wall-clock cap (same guardrail as bench-agent.sh). cursor-agent
  # headless: -p prompt, stream-json events on stdout, --force to skip
  # interactive approval, --workspace to scope it to the fixture repo.
  timeout --kill-after=10s "${BENCH_TASK_TIMEOUT:-600}" \
    "$CURSOR_BIN" -p "$prompt" \
    --output-format stream-json --model "$MODEL" --force \
    --workspace "$WORKSPACE" \
    > "$raw_file" 2> "$err_file" || rc=$?
  if [ "$rc" = "124" ] || [ "$rc" = "137" ]; then
    echo "       WARN: $task_id hit BENCH_TASK_TIMEOUT (${BENCH_TASK_TIMEOUT:-600}s) — killed"
  fi
  end_s="$(date +%s)"
  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  wall_ms=$(( (end_s - start_s) * 1000 ))

  [ "$rc" -ne 0 ] && echo "       WARN: cursor-agent exited $rc — see $err_file"
  [ -s "$raw_file" ] || echo "       WARN: no output — see $err_file"

  # Token accounting (shared normalizer, cursor branch). tokenlog is "-": cursor
  # reports usage inline in its result event, no separate token log.
  python3 "$SCRIPT_DIR/extract_tokens.py" cursor "$raw_file" "-" \
    "$task_id" "$task_name" "$MODEL" "$MODEL" \
    "$started_at" "$ended_at" "$wall_ms" "$rc" "$out_file"

  # Correctness gate (shared checker; cursor emits a stream-json event stream,
  # so use the default format — the response lives in the `result` event).
  python3 "$SCRIPT_DIR/check_task.py" "$task_file" "$raw_file" "$WORKSPACE" "$out_file" stream-json \
    || echo "       WARN: check_task.py failed for $task_id"
}

for task_file in "$TASKS_DIR"/*.json; do
  [ -e "$task_file" ] || { echo "No tasks found in $TASKS_DIR"; exit 1; }
  run_task "$task_file"
done

reset_workspace

echo
echo "=== summary ($RUN_ID) ==="
python3 "$SCRIPT_DIR/summarize.py" "$RUN_DIR"

echo
echo "Per-task JSON: $RUN_DIR/*.json"
echo "cost_usd is null (cursor-agent emits no cost); this is token+correctness parity only."
