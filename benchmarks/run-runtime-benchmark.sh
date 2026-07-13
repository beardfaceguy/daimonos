#!/bin/sh
# Runtime token benchmark: run identical tasks on ONE agent runtime and record
# normalized token usage. Runtimes are whole agent stacks, each with its own
# native tools:
#   daimonos  -> `daimonos agent` (native daimonos tools, provider from bench env)
#   claude    -> Claude Code CLI  (native built-in tools)
#   cursor    -> cursor-agent CLI (native built-in tools)
#
# This is a DIFFERENT comparison from run-benchmark.sh (which runs the Claude CLI
# for every arm and only toggles the daimonos MCP on/off). Do not conflate them.
#
# POSIX sh (works with BusyBox ash). Model slugs come from models.json so adding
# Opus/fable is a one-line edit there.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"
MODELS_JSON="$SCRIPT_DIR/models.json"

CLAUDE="${CLAUDE_BIN:-$HOME/.local/bin/claude}"
CURSOR="${CURSOR_BIN:-$HOME/.local/bin/cursor-agent}"
DAIMONOS_BIN="${DAIMONOS_BIN:-$SCRIPT_DIR/../target/release/daimonos}"

CANON="${BENCH_CANON_MODEL:-sonnet}"
RUNS="${BENCH_RUNS:-1}"
RUN_TAG="${BENCH_TAG:-}"

# daimonos --debug-tokens appends here (fixed path, not configurable).
TOKEN_LOG="$HOME/.config/daimonos/token-debug.log"
# Bench agent env: a copy of the user's agent.env with APPROVAL_MODE=auto so the
# agent runs headless. Kept under ~/.config/daimonos (NOT the repo) since it
# carries the API key. Regenerated each invocation from the source env.
SRC_AGENT_ENV="${DAIMONOS_AGENT_ENV:-$HOME/.config/daimonos/agent.env}"
BENCH_AGENT_ENV="${DAIMONOS_BENCH_ENV:-$HOME/.config/daimonos/agent.bench.env}"

RUNTIME="${1:-}"
TASK_FILTER="${2:-}"

usage() {
  echo "Usage: $0 <daimonos|claude|cursor> [task-id]"
  echo ""
  echo "Environment variables:"
  echo "  BENCH_CANON_MODEL  Canonical model from models.json (default: sonnet)"
  echo "  BENCH_RUNS         Repeat the suite N times (default: 1)"
  echo "  BENCH_TAG          Extra tag for the run directory name"
  echo "  CLAUDE_BIN         Path to claude CLI (default: ~/.local/bin/claude)"
  echo "  CURSOR_BIN         Path to cursor-agent (default: ~/.local/bin/cursor-agent)"
  echo "  DAIMONOS_BIN       Path to daimonos (default: ../target/release/daimonos)"
  exit 1
}

[ -n "$RUNTIME" ] || usage
case "$RUNTIME" in
  daimonos | claude | cursor) ;;
  *) echo "Error: runtime must be daimonos, claude, or cursor"; exit 1 ;;
esac

# Resolve the runtime-specific model slug (or bail if unavailable for this runtime).
MODEL_SLUG="$(node -e '
var m = require(process.argv[1]);
var entry = m[process.argv[2]];
if (!entry) { console.error("unknown canonical model: " + process.argv[2]); process.exit(3); }
var slug = entry[process.argv[3]];
if (slug === null) { console.error("SKIP"); process.exit(4); }
if (!slug) { console.error("no slug for runtime " + process.argv[3]); process.exit(3); }
process.stdout.write(slug);
' "$MODELS_JSON" "$CANON" "$RUNTIME")" || {
  rc=$?
  if [ "$rc" = "4" ]; then
    echo "Model '$CANON' is not available for runtime '$RUNTIME' (null slug in models.json) — nothing to run."
    exit 0
  fi
  echo "Error resolving model slug for $RUNTIME/$CANON"
  exit 1
}

# API key wiring for the Anthropic-backed runtimes.
if [ "$RUNTIME" = "claude" ]; then
  if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    API_KEY_FILE="$SCRIPT_DIR/../claude_api_key.env"
    if [ -f "$API_KEY_FILE" ]; then
      # shellcheck disable=SC1090
      . "$API_KEY_FILE"
      export ANTHROPIC_API_KEY
    else
      echo "Error: ANTHROPIC_API_KEY not set and $API_KEY_FILE not found"
      exit 1
    fi
  fi
fi

# Bench agent env for daimonos (headless auto-approve).
if [ "$RUNTIME" = "daimonos" ]; then
  [ -f "$SRC_AGENT_ENV" ] || { echo "Error: no agent env at $SRC_AGENT_ENV"; exit 1; }
  sed 's/^DAIMONOS_AGENT_APPROVAL_MODE=.*/DAIMONOS_AGENT_APPROVAL_MODE=auto/' \
    "$SRC_AGENT_ENV" > "$BENCH_AGENT_ENV"
  # Force compaction off: bench tasks are short (no eviction needed) and off
  # avoids needing a resolved context window. DAIMONOS_AGENT_COMPACTION is a
  # required key, so add it if the source env omits it.
  if grep -q '^DAIMONOS_AGENT_COMPACTION=' "$BENCH_AGENT_ENV"; then
    sed -i 's/^DAIMONOS_AGENT_COMPACTION=.*/DAIMONOS_AGENT_COMPACTION=off/' "$BENCH_AGENT_ENV"
  else
    printf 'DAIMONOS_AGENT_COMPACTION=off\n' >> "$BENCH_AGENT_ENV"
  fi
  chmod 600 "$BENCH_AGENT_ENV"
fi

json_field() {
  node -e 'var d=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));console.log(d[process.argv[2]]||"")' "$1" "$2"
}

json_array_join() {
  node -e 'var d=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));console.log((d[process.argv[2]]||[]).join(","))' "$1" "$2"
}

reset_workspace() {
  cd "$WORKSPACE"
  git checkout -- . 2>/dev/null || true
  git clean -fd -e .cursor/ 2>/dev/null || true
}

run_task() {
  task_file="$1"
  task_id="$(json_field "$task_file" id)"
  task_name="$(json_field "$task_file" name)"
  applies_to="$(json_array_join "$task_file" applies_to)"
  prompt="$(json_field "$task_file" prompt)"

  if [ -n "$TASK_FILTER" ]; then
    case "$task_id" in
      "$TASK_FILTER"*) ;;
      *) return 0 ;;
    esac
  fi

  # The three runtimes are general agents; a task applies if it's a generic
  # (non-daimonos-only) task. Task 07 (snapshot) is daimonos-tool-specific and
  # has no cross-runtime meaning, so it is excluded from all arms here.
  case "$applies_to" in
    *cursor*) ;;
    *)
      echo "  SKIP $task_id ($task_name) — daimonos-only, not a cross-runtime task"
      return 0
      ;;
  esac

  echo "  RUN  $task_id: $task_name"
  reset_workspace

  out_file="$RUN_DIR/${task_id}.json"
  tokenlog_file="$RUN_DIR/${task_id}.tokenlog.jsonl"
  fmt="stream-json"

  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  start_s="$(date +%s)"

  cd "$WORKSPACE"
  case "$RUNTIME" in
    claude)
      raw_file="$RUN_DIR/${task_id}.raw.jsonl"
      # Native built-in tools only: no --mcp-config, plus --strict-mcp-config so
      # a user-scope MCP registration can't contaminate the runtime (vikunja #926).
      printf '%s' "$prompt" | "$CLAUDE" -p --output-format stream-json --verbose \
        --model "$MODEL_SLUG" --dangerously-skip-permissions \
        --no-session-persistence --strict-mcp-config \
        > "$raw_file" 2>/dev/null || true
      ;;
    cursor)
      raw_file="$RUN_DIR/${task_id}.raw.jsonl"
      printf '%s' "$prompt" | "$CURSOR" -p --output-format stream-json \
        --model "$MODEL_SLUG" --force \
        > "$raw_file" 2>/dev/null || true
      ;;
    daimonos)
      raw_file="$RUN_DIR/${task_id}.raw.txt"
      fmt="text"
      # Capture the token-log offset so we read only THIS run's call lines.
      pre_lines=0
      [ -f "$TOKEN_LOG" ] && pre_lines="$(wc -l < "$TOKEN_LOG" | tr -d ' ')"
      "$DAIMONOS_BIN" --debug-tokens -w "$WORKSPACE" agent "$prompt" \
        --model "$MODEL_SLUG" --agent-env "$BENCH_AGENT_ENV" \
        > "$raw_file" 2>/dev/null || true
      if [ -f "$TOKEN_LOG" ]; then
        tail -n "+$((pre_lines + 1))" "$TOKEN_LOG" > "$tokenlog_file" || true
      else
        : > "$tokenlog_file"
      fi
      ;;
  esac

  end_s="$(date +%s)"
  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  wall_ms=$(( (end_s - start_s) * 1000 ))

  node "$SCRIPT_DIR/extract-tokens.js" "$RUNTIME" "$raw_file" "$tokenlog_file" \
    "$task_id" "$task_name" "$MODEL_SLUG" "$CANON" \
    "$started_at" "$ended_at" "$wall_ms" "$out_file"

  # Correctness gate: response checks read the transcript in the runtime's
  # format; workspace checks (filesystem ground truth) are format-agnostic.
  node "$SCRIPT_DIR/check-task.js" "$task_file" "$raw_file" "$WORKSPACE" "$out_file" "$fmt" \
    || echo "       WARN: check-task.js failed for $task_id"
}

run_number=1
while [ "$run_number" -le "$RUNS" ]; do
  if [ "$RUNS" -gt 1 ]; then
    if [ -n "$RUN_TAG" ]; then
      effective_tag="${RUN_TAG}-r${run_number}"
    else
      effective_tag="r${run_number}"
    fi
  else
    effective_tag="$RUN_TAG"
  fi

  if [ -n "$effective_tag" ]; then
    RUN_ID="$(date +%Y%m%d-%H%M%S)-${RUNTIME}-${CANON}-${effective_tag}"
  else
    RUN_ID="$(date +%Y%m%d-%H%M%S)-${RUNTIME}-${CANON}"
  fi
  RUN_DIR="$RESULTS_DIR/$RUN_ID"
  mkdir -p "$RUN_DIR"

  echo "=== Runtime Benchmark ($run_number/$RUNS) ==="
  echo "Runtime:   $RUNTIME"
  echo "Model:     $CANON -> $MODEL_SLUG"
  echo "Run ID:    $RUN_ID"
  echo "Workspace: $WORKSPACE"
  echo ""

  for task_file in "$TASKS_DIR"/*.json; do
    run_task "$task_file"
  done

  echo ""
  run_number=$((run_number + 1))
done

echo "=== Done ==="
echo "Analyze:  python3 $SCRIPT_DIR/analyze-runtimes.py $RESULTS_DIR"
if [ "$RUNTIME" = "cursor" ]; then
  echo "Cursor cost: after the admin report populates, run:"
  echo "  python3 $SCRIPT_DIR/cursor-attribute.py $RUN_DIR <team-usage-events.csv>"
fi
