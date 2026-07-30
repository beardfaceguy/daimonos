#!/bin/sh
# bench-codex.sh — the Codex-CLI side of the harness comparison.
#
# Runs the same task suite in `benchmarks/tasks/` through OpenAI's `codex exec`
# (headless), routed THROUGH OPENROUTER at the same model + reasoning effort
# daimonos uses, so the benchmark isolates the *harness* (daimonos's agent loop
# vs Codex's) while holding model, effort, serving layer, and billing account
# constant. This is the controlled counterpart to bench-agent.sh; unlike the
# cursor arm, cost IS comparable here (both bill the same OpenRouter account).
#
# Preconditions (set up once, outside this script):
#   ~/.codex/config.toml must route through OpenRouter, e.g.:
#     model = "openai/gpt-5.6-sol"
#     model_provider = "openrouter"
#     model_reasoning_effort = "high"
#     [model_providers.openrouter]
#     base_url = "https://openrouter.ai/api/v1"
#     env_key = "OPENROUTER_API_KEY"
#     wire_api = "responses"
#   The OpenRouter key is NOT stored in config.toml; this script exports
#   OPENROUTER_API_KEY at run time. By default it reads a DEDICATED benchmark
#   key (see CODEX_OPENROUTER_KEY_FILE) so the Codex side is trivially
#   filterable in the OpenRouter console, distinct from daimonos's own key.
#
# Reuses extract_tokens.py (codex branch) and check_task.py (text format, fed
# Codex's `-o` last-message file) unchanged; only the runner differs. Emits
# per-task summary JSONs in the identical schema as bench-agent.sh so a single
# analyze.py run compares both sides.
#
# Usage:
#   ./bench-codex.sh [task-id-prefix]
#
#   task-id-prefix   Optional: run only tasks whose id starts with this
#                    (e.g. "01" for a single-task smoke test). Omit to run all.
#
# Environment variables:
#   CODEX_MODEL    Model slug passed to `codex -m` and stamped in the report
#                  (default: read DAIMONOS_AGENT_MODEL from agent.env, so both
#                  sides report the same slug). config.toml's model is the
#                  effective default; -m makes the report unambiguous.
#   BENCH_TAG      Label folded into the results dir name (e.g. sol-high-r1).
#   CODEX_BIN      Path to the codex binary (default: from PATH).
#   CODEX_OPENROUTER_KEY_FILE  File holding the OpenRouter key for the Codex
#                  side, as a bare key value or a KEY=value / OPENROUTER_API_KEY=
#                  line (default: ~/.blue_rose/openrouter_api_key_codex_benchmarking.env).
#                  Using a dedicated key isolates Codex spend in the console.
#   DAIMONOS_AGENT_ENV  agent.env to source the MODEL SLUG from, so both sides
#                  report the same model (default: ~/.config/daimonos/agent.env).
#   BENCH_TASK_TIMEOUT  Per-task wall-clock cap in seconds (default: 600), same
#                  guardrail as bench-agent.sh: a stuck task must not run away.
#
# Portability: POSIX sh syntax, but requires GNU coreutils `timeout`, `node`,
# and `codex` on PATH. Targets the Linux benchmark host, not BusyBox.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"

if [ -n "${CODEX_BIN:-}" ]; then
  :
elif command -v codex >/dev/null 2>&1; then
  CODEX_BIN="$(command -v codex)"
else
  CODEX_BIN="$HOME/.local/bin/codex"
fi

SRC_AGENT_ENV="${DAIMONOS_AGENT_ENV:-$HOME/.config/daimonos/agent.env}"
CODEX_KEY_FILE="${CODEX_OPENROUTER_KEY_FILE:-$HOME/.blue_rose/openrouter_api_key_codex_benchmarking.env}"
TASK_FILTER="${1:-}"
RUN_TAG="${BENCH_TAG:-}"

[ -x "$CODEX_BIN" ] || { echo "Error: codex not found/executable at $CODEX_BIN"; exit 1; }
[ -f "$SRC_AGENT_ENV" ] || { echo "Error: no agent env at $SRC_AGENT_ENV (needed for the model slug)"; exit 1; }
[ -f "$CODEX_KEY_FILE" ] || { echo "Error: no Codex OpenRouter key file at $CODEX_KEY_FILE"; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "Error: python3 is required (token extraction + correctness checks)"; exit 1; }
command -v timeout >/dev/null 2>&1 || { echo "Error: GNU coreutils 'timeout' is required for the per-task wall-clock cap"; exit 1; }

env_val() {
  grep -E "^$1=" "$SRC_AGENT_ENV" | tail -1 | sed -E "s/^$1=//; s/^\"//; s/\"\$//"
}

# The Codex side bills a DEDICATED OpenRouter key (isolates its spend in the
# console, separate from daimonos's key). The file may be a bare key value or a
# `KEY=value` / `OPENROUTER_API_KEY=value` line; accept either. Read the first
# non-empty, non-comment line and strip an optional `NAME=` prefix + quotes.
OPENROUTER_API_KEY="$(
  grep -vE '^[[:space:]]*(#|$)' "$CODEX_KEY_FILE" \
    | head -1 \
    | sed -E 's/^[A-Za-z_][A-Za-z0-9_]*=//; s/^"//; s/"$//; s/^[[:space:]]+//; s/[[:space:]]+$//'
)"
[ -n "$OPENROUTER_API_KEY" ] || { echo "Error: no key found in $CODEX_KEY_FILE"; exit 1; }
export OPENROUTER_API_KEY

FILE_MODEL="$(env_val DAIMONOS_AGENT_MODEL)"
MODEL="${CODEX_MODEL:-$FILE_MODEL}"
[ -n "$MODEL" ] || { echo "Error: no CODEX_MODEL and no DAIMONOS_AGENT_MODEL in $SRC_AGENT_ENV"; exit 1; }

RUN_ID="$(date +%Y%m%d-%H%M%S)-codex${RUN_TAG:+-$RUN_TAG}"
RUN_DIR="$RESULTS_DIR/$RUN_ID"
mkdir -p "$RUN_DIR"

echo "=== codex-cli benchmark (via OpenRouter) ==="
echo "Model:    $MODEL   (codex exec, model_provider=openrouter)"
echo "Binary:   $CODEX_BIN"
echo "Run dir:  $RUN_DIR"
echo "Key:      dedicated Codex key from $CODEX_KEY_FILE (isolated in OpenRouter console)"
[ -n "$TASK_FILTER" ] && echo "Filter:   tasks matching '$TASK_FILTER'*"
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

  # Reuse the "cursor" applicability flag: these tasks apply to any external
  # coding-agent CLI. (Tasks tag daimonos + cursor; codex is the same class.)
  case ",$applies_to," in
    *",cursor,"*|*",codex,"*) ;;
    *) echo "  SKIP $task_id ($task_name) — not applicable to an external agent CLI"; return 0 ;;
  esac

  echo "  RUN  $task_id: $task_name"
  reset_workspace

  out_file="$RUN_DIR/${task_id}.json"
  raw_file="$RUN_DIR/${task_id}.raw.jsonl"
  last_file="$RUN_DIR/${task_id}.last.txt"
  err_file="$RUN_DIR/${task_id}.stderr.log"

  start_s="$(date +%s)"
  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  rc=0
  cd "$WORKSPACE"
  # Headless codex: --json event stream to stdout, final message to -o (plain
  # text, fed to the correctness gate). Bypass approvals+sandbox because the
  # suite includes shell/git/cargo tasks and must run unattended; the workspace
  # is a throwaway fixture reset between tasks. Per-task wall-clock cap matches
  # bench-agent.sh so a stuck task can't burn credits unbounded.
  timeout --kill-after=10s "${BENCH_TASK_TIMEOUT:-600}" \
    "$CODEX_BIN" exec "$prompt" \
    --json -m "$MODEL" -C "$WORKSPACE" \
    --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check \
    -o "$last_file" \
    > "$raw_file" 2> "$err_file" || rc=$?
  if [ "$rc" = "124" ] || [ "$rc" = "137" ]; then
    echo "       WARN: $task_id hit BENCH_TASK_TIMEOUT (${BENCH_TASK_TIMEOUT:-600}s) — killed"
  fi
  end_s="$(date +%s)"
  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  wall_ms=$(( (end_s - start_s) * 1000 ))

  [ "$rc" -ne 0 ] && echo "       WARN: codex exited $rc — see $err_file"
  [ -s "$raw_file" ] || echo "       WARN: no event output — see $err_file"

  # Token accounting (shared normalizer, codex branch reads turn.completed
  # usage from the event stream). tokenlog is "-": codex carries usage inline.
  python3 "$SCRIPT_DIR/extract_tokens.py" codex "$raw_file" "-" \
    "$task_id" "$task_name" "$MODEL" "$MODEL" \
    "$started_at" "$ended_at" "$wall_ms" "$rc" "$out_file"

  # Correctness gate. Codex's -o file is the clean final assistant message as
  # plain text, so use the "text" format (same as daimonos) rather than
  # re-parsing the event stream. Fall back to an empty file if -o wrote nothing.
  [ -f "$last_file" ] || : > "$last_file"
  python3 "$SCRIPT_DIR/check_task.py" "$task_file" "$last_file" "$WORKSPACE" "$out_file" text \
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
echo "Same model + effort as bench-agent.sh (both via OpenRouter); dedicated Codex key isolates its spend. Harness is the variable."
