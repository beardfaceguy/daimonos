#!/bin/sh
# bench-agent.sh — the single daimonos agent-mode benchmark.
#
# Runs the task suite in `benchmarks/tasks/` through `daimonos agent` (daimonos
# IS the agent, using its own native tools), records per-task tokens / cost /
# wall time and a machine-checkable correctness verdict, and prints a summary.
#
# Model + provider + key come from ONE place: the agent env file
# (`~/.config/daimonos/agent.env` by default). Whatever DAIMONOS_AGENT_MODEL /
# PROVIDER / API_KEY that file holds is what runs and what gets billed. There is
# no models.json and no --model override baked in: the file is the single source
# of truth. (An explicit MODEL=<slug> env var can still override for a one-off,
# but the default is "run exactly what agent.env says".)
#
# Usage:
#   ./bench-agent.sh [task-id-prefix]
#
#   task-id-prefix   Optional: run only tasks whose id starts with this
#                    (e.g. "01" for a single-task smoke test). Omit to run all.
#
# Environment variables:
#   BENCH_TAG      Extra label folded into the results dir name.
#   MODEL          Override the agent.env model for this run only (rarely needed).
#   DAIMONOS_BIN   Path to the daimonos binary (default: ../target/release/daimonos).
#   DAIMONOS_AGENT_ENV  Source agent env file (default: ~/.config/daimonos/agent.env).
#   BENCH_BUILD    If "1", cargo build --release first (default: 0 — measure the
#                  binary as-is; set to 1 to guarantee current code).
#
# POSIX sh compatible (works with BusyBox ash).
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"
DAIMONOS_BIN="${DAIMONOS_BIN:-$SCRIPT_DIR/../target/release/daimonos}"

# daimonos --debug-tokens appends here (fixed path in the binary, not configurable).
TOKEN_LOG="$HOME/.config/daimonos/token-debug.log"

# The agent env file is the single source of truth for model/provider/key.
SRC_AGENT_ENV="${DAIMONOS_AGENT_ENV:-$HOME/.config/daimonos/agent.env}"

TASK_FILTER="${1:-}"
RUN_TAG="${BENCH_TAG:-}"

[ -f "$SRC_AGENT_ENV" ] || { echo "Error: no agent env at $SRC_AGENT_ENV"; exit 1; }
[ -x "$DAIMONOS_BIN" ] || { echo "Error: daimonos binary not found/executable at $DAIMONOS_BIN"; exit 1; }
command -v node >/dev/null 2>&1 || { echo "Error: node is required (token extraction + correctness checks)"; exit 1; }

if [ "${BENCH_BUILD:-0}" = "1" ]; then
  echo "--- release build ---"
  ( cd "$SCRIPT_DIR/.." && cargo build --release ) || { echo "RESULT: BUILD_FAIL"; exit 1; }
fi

# Resolve the model to display/report. By default read it straight from the
# agent env file so the report names what actually ran; MODEL overrides only if
# the caller set it explicitly (and is then also passed through as --model).
FILE_MODEL="$(grep -E '^DAIMONOS_AGENT_MODEL=' "$SRC_AGENT_ENV" | tail -1 | sed -E 's/^DAIMONOS_AGENT_MODEL=//; s/^"//; s/"$//')"
MODEL="${MODEL:-$FILE_MODEL}"
[ -n "$MODEL" ] || { echo "Error: no DAIMONOS_AGENT_MODEL in $SRC_AGENT_ENV and no MODEL override"; exit 1; }

# Build the bench agent env: a copy of the source with APPROVAL_MODE=auto (so the
# agent never blocks on a permission prompt mid-suite) and COMPACTION=off (so a
# run needs no resolved context window). The key/provider/model are inherited
# unchanged from the source file — same billing as interactive use.
BENCH_AGENT_ENV="$(mktemp "${TMPDIR:-/tmp}/daimonos-bench-env.XXXXXX")"
trap 'rm -f "$BENCH_AGENT_ENV"' EXIT INT TERM
sed 's/^DAIMONOS_AGENT_APPROVAL_MODE=.*/DAIMONOS_AGENT_APPROVAL_MODE=auto/' "$SRC_AGENT_ENV" > "$BENCH_AGENT_ENV"
grep -q '^DAIMONOS_AGENT_APPROVAL_MODE=' "$BENCH_AGENT_ENV" || printf 'DAIMONOS_AGENT_APPROVAL_MODE=auto\n' >> "$BENCH_AGENT_ENV"
if grep -q '^DAIMONOS_AGENT_COMPACTION=' "$BENCH_AGENT_ENV"; then
  sed 's/^DAIMONOS_AGENT_COMPACTION=.*/DAIMONOS_AGENT_COMPACTION=off/' "$BENCH_AGENT_ENV" > "$BENCH_AGENT_ENV.tmp"
  mv "$BENCH_AGENT_ENV.tmp" "$BENCH_AGENT_ENV"
else
  [ -s "$BENCH_AGENT_ENV" ] && [ -n "$(tail -c 1 "$BENCH_AGENT_ENV")" ] && printf '\n' >> "$BENCH_AGENT_ENV"
  printf 'DAIMONOS_AGENT_COMPACTION=off\n' >> "$BENCH_AGENT_ENV"
fi
chmod 600 "$BENCH_AGENT_ENV"

RUN_ID="$(date +%Y%m%d-%H%M%S)-agent${RUN_TAG:+-$RUN_TAG}"
RUN_DIR="$RESULTS_DIR/$RUN_ID"
mkdir -p "$RUN_DIR"

echo "=== daimonos agent-mode benchmark ==="
echo "Model:    $MODEL   (from $([ "$MODEL" = "$FILE_MODEL" ] && echo "$SRC_AGENT_ENV" || echo 'MODEL override'))"
echo "Binary:   $DAIMONOS_BIN"
echo "Run dir:  $RUN_DIR"
[ -n "$TASK_FILTER" ] && echo "Filter:   tasks matching '$TASK_FILTER'*"
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

  # Only run tasks that declare they apply to daimonos.
  case ",$applies_to," in
    *",daimonos,"*) ;;
    *) echo "  SKIP $task_id ($task_name) — not applicable to daimonos"; return 0 ;;
  esac

  echo "  RUN  $task_id: $task_name"
  reset_workspace

  out_file="$RUN_DIR/${task_id}.json"
  raw_file="$RUN_DIR/${task_id}.raw.txt"
  err_file="$RUN_DIR/${task_id}.stderr.log"
  tokenlog_file="$RUN_DIR/${task_id}.tokenlog.jsonl"

  # Capture the token-log offset so we read only THIS run's call lines
  # (extract-tokens.js further filters the delta by this task's time window,
  # so a concurrent daimonos process can't contaminate the sums).
  pre_lines=0
  [ -f "$TOKEN_LOG" ] && pre_lines="$(wc -l < "$TOKEN_LOG" | tr -d ' ')"

  start_s="$(date +%s)"
  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  rc=0
  cd "$WORKSPACE"
  # Per-task wall-clock cap: with an uncapped auto-top-up billing account, a
  # stuck/looping task must not burn credits unbounded. `timeout` sends TERM at
  # the cap, then KILL shortly after; a timed-out task exits non-zero and is
  # recorded as an error (excluded from aggregates by the correctness gate).
  timeout --kill-after=10s "${BENCH_TASK_TIMEOUT:-600}" \
    "$DAIMONOS_BIN" --debug-tokens -w "$WORKSPACE" agent "$prompt" \
    --model "$MODEL" --agent-env "$BENCH_AGENT_ENV" \
    > "$raw_file" 2> "$err_file" || rc=$?
  if [ "$rc" = "124" ] || [ "$rc" = "137" ]; then
    echo "       WARN: $task_id hit BENCH_TASK_TIMEOUT (${BENCH_TASK_TIMEOUT:-600}s) — killed"
  fi
  end_s="$(date +%s)"
  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  wall_ms=$(( (end_s - start_s) * 1000 ))

  if [ -f "$TOKEN_LOG" ]; then
    tail -n "+$((pre_lines + 1))" "$TOKEN_LOG" > "$tokenlog_file" || true
  else
    : > "$tokenlog_file"
  fi

  [ "$rc" -ne 0 ] && echo "       WARN: daimonos exited $rc — see $err_file"
  [ -s "$raw_file" ] || echo "       WARN: no output — see $err_file"

  # Token accounting (reuses the shared normalizer's daimonos branch).
  node "$SCRIPT_DIR/extract-tokens.js" daimonos "$raw_file" "$tokenlog_file" \
    "$task_id" "$task_name" "$MODEL" "$MODEL" \
    "$started_at" "$ended_at" "$wall_ms" "$rc" "$out_file"

  # Correctness gate (reuses the shared checker; daimonos stdout is the response).
  node "$SCRIPT_DIR/check-task.js" "$task_file" "$raw_file" "$WORKSPACE" "$out_file" text \
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
let ti = 0, to = 0, tc = 0, tw = 0, correct = 0, checked = 0;
for (const r of rows) {
  const i = r.input || 0, o = r.output || 0;
  const c = r.cost_usd || 0, w = (r.wall_ms || 0) / 1000;
  ti += i; to += o; tc += c; tw += w;
  let verdict = r.correct === null || r.correct === undefined ? "—"
    : (r.correct ? "OK" : "INCORRECT");
  if (r.correct === true) correct++;
  if (r.correct === true || r.correct === false) checked++;
  const chk = (r.checks_passed ?? "?") + "/" + (r.checks_total ?? "?");
  console.log([pad(r.task_id, 22), rpad(i, 9), rpad(o, 8), rpad(c.toFixed(4), 9), rpad(w.toFixed(1), 7), "  " + verdict + " (" + chk + ")"].join(""));
}
console.log("-".repeat(60));
console.log([pad("TOTAL " + rows.length + " tasks", 22), rpad(ti, 9), rpad(to, 8), rpad(tc.toFixed(4), 9), rpad(tw.toFixed(1), 7), "  " + correct + "/" + checked + " correct"].join(""));
' "$RUN_DIR"

echo
echo "Per-task JSON: $RUN_DIR/*.json"
