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
# It reuses `extract-tokens.js` (cursor branch) and `check-task.js`
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
command -v node >/dev/null 2>&1 || { echo "Error: node is required (token extraction + correctness checks)"; exit 1; }
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
  node -e "var d=JSON.parse(require('fs').readFileSync(process.argv[1],'utf8'));process.stdout.write(String(d[process.argv[2]]||''))" "$1" "$2"
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
  applies_to="$(node -e "var d=JSON.parse(require('fs').readFileSync(process.argv[1],'utf8'));process.stdout.write((d.applies_to||[]).join(','))" "$task_file")"
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
  node "$SCRIPT_DIR/extract-tokens.js" cursor "$raw_file" "-" \
    "$task_id" "$task_name" "$MODEL" "$MODEL" \
    "$started_at" "$ended_at" "$wall_ms" "$rc" "$out_file"

  # Correctness gate (shared checker; cursor emits a stream-json event stream,
  # so use the default format — the response lives in the `result` event).
  node "$SCRIPT_DIR/check-task.js" "$task_file" "$raw_file" "$WORKSPACE" "$out_file" stream-json \
    || echo "       WARN: check-task.js failed for $task_id"
}

for task_file in "$TASKS_DIR"/*.json; do
  [ -e "$task_file" ] || { echo "No tasks found in $TASKS_DIR"; exit 1; }
  run_task "$task_file"
done

reset_workspace

echo
echo "=== summary ($RUN_ID) ==="
node -e '
const fs = require("fs"), path = require("path");
const dir = process.argv[1];
const rows = fs.readdirSync(dir)
  .filter(f => f.endsWith(".json"))
  .map(f => JSON.parse(fs.readFileSync(path.join(dir, f), "utf8")))
  .sort((a, b) => String(a.task_id).localeCompare(String(b.task_id)));
if (!rows.length) { console.log("(no task summaries)"); process.exit(0); }
const pad = (s, n) => String(s).padEnd(n);
const rpad = (s, n) => String(s).padStart(n);
console.log([pad("task", 22), rpad("in", 9), rpad("out", 8), rpad("cost$", 9), rpad("wall_s", 7), "  correct"].join(""));
let ti = 0, to = 0, tw = 0, correct = 0, checked = 0;
for (const r of rows) {
  const i = r.input || 0, o = r.output || 0;
  const w = (r.wall_ms || 0) / 1000;
  ti += i; to += o; tw += w;
  let verdict = r.correct === null || r.correct === undefined ? "—"
    : (r.correct ? "OK" : "INCORRECT");
  if (r.correct === true) correct++;
  if (r.correct === true || r.correct === false) checked++;
  const chk = (r.checks_passed ?? "?") + "/" + (r.checks_total ?? "?");
  const costStr = r.cost_usd === null || r.cost_usd === undefined ? "n/a" : (r.cost_usd).toFixed(4);
  console.log([pad(r.task_id, 22), rpad(i, 9), rpad(o, 8), rpad(costStr, 9), rpad(w.toFixed(1), 7), "  " + verdict + " (" + chk + ")"].join(""));
}
console.log("-".repeat(60));
console.log([pad("TOTAL " + rows.length + " tasks", 22), rpad(ti, 9), rpad(to, 8), rpad("n/a", 9), rpad(tw.toFixed(1), 7), "  " + correct + "/" + checked + " correct"].join(""));
' "$RUN_DIR"

echo
echo "Per-task JSON: $RUN_DIR/*.json"
echo "cost_usd is null (cursor-agent emits no cost); this is token+correctness parity only."
