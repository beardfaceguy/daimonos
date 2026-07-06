#!/bin/sh
# Runs benchmark tasks using the Claude CLI.
# POSIX sh compatible (works with BusyBox ash).
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"
CLAUDE="${CLAUDE_BIN:-$HOME/.local/bin/claude}"
MODEL="${BENCH_MODEL:-opus}"
DAIMONOS_BIN="${DAIMONOS_BIN:-$SCRIPT_DIR/../target/release/daimonos}"
MCP_CONFIG="$WORKSPACE/.cursor/mcp.json"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  API_KEY_FILE="$SCRIPT_DIR/../claude_api_key.env"
  if [ -f "$API_KEY_FILE" ]; then
    . "$API_KEY_FILE"
    export ANTHROPIC_API_KEY
  else
    echo "Error: ANTHROPIC_API_KEY not set and $API_KEY_FILE not found"
    exit 1
  fi
fi

MODE="${1:-}"
TASK_FILTER="${2:-}"
RUN_TAG="${BENCH_TAG:-}"
RUNS="${BENCH_RUNS:-1}"

if [ -z "$MODE" ]; then
  echo "Usage: $0 <baseline|baseline-terse|daimonos> [task-id]"
  echo ""
  echo "Environment variables:"
  echo "  BENCH_RUNS      Repeat the suite N times, one -rN-tagged run dir each (default: 1)"
  echo "  BENCH_MODEL     Model alias or slug (default: opus)"
  echo "  CLAUDE_BIN      Path to claude CLI (default: ~/.local/bin/claude)"
  echo "  DAIMONOS_BIN    Path to daimonos binary (default: ../target/release/daimonos)"
  exit 1
fi

if [ "$MODE" != "baseline" ] && [ "$MODE" != "baseline-terse" ] && [ "$MODE" != "daimonos" ]; then
  echo "Error: mode must be 'baseline', 'baseline-terse', or 'daimonos'"
  exit 1
fi

# RUN_ID/RUN_DIR are (re)computed per repetition inside the driver loop at
# the bottom of this file; with BENCH_RUNS>1 each repetition gets its own
# -rN-suffixed run directory so the analyzer can aggregate across them.
RUN_ID=""
RUN_DIR=""

# Use node for JSON parsing (available on both Ubuntu and daimonos)
json_field() {
  node -e "var d=JSON.parse(require('fs').readFileSync(process.argv[1],'utf8'));console.log(d[process.argv[2]]||'')" "$1" "$2"
}

json_array_join() {
  node -e "var d=JSON.parse(require('fs').readFileSync(process.argv[1],'utf8'));console.log((d[process.argv[2]]||[]).join(','))" "$1" "$2"
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

  check_mode="$MODE"
  case "$check_mode" in baseline*) check_mode="cursor" ;; esac
  case "$applies_to" in
    *"$check_mode"*) ;;
    *)
      echo "  SKIP $task_id ($task_name) — not applicable to $MODE"
      return 0
      ;;
  esac

  echo "  RUN  $task_id: $task_name"
  reset_workspace

  out_file="$RUN_DIR/${task_id}.json"
  raw_file="$RUN_DIR/${task_id}.raw.jsonl"

  # Build claude CLI arguments (POSIX-compatible, no bash arrays)
  CLAUDE_ARGS="-p --output-format stream-json --verbose --model $MODEL"
  CLAUDE_ARGS="$CLAUDE_ARGS --dangerously-skip-permissions --no-session-persistence --bare"
  # Arm isolation: every arm gets --strict-mcp-config so only servers named in
  # an explicit --mcp-config load. Baseline arms pass no --mcp-config, which
  # guarantees zero MCP servers — without this, a user-scope daimonos
  # registration would silently contaminate the baseline arm (the exact bug
  # that produced ponytail's false ~4% result; see vikunja #926).
  CLAUDE_ARGS="$CLAUDE_ARGS --strict-mcp-config"

  if [ "$MODE" = "daimonos" ]; then
    CLAUDE_ARGS="$CLAUDE_ARGS --mcp-config $MCP_CONFIG"
    task_category="$(json_field "$task_file" category)"
    if [ "$task_category" = "exec_filter" ]; then
      # exec_filter tasks: force exec tool usage to exercise L1/L2 filtering
      CLAUDE_ARGS="$CLAUDE_ARGS --append-system-prompt \"Use the daimonos exec tool to run shell commands. Do NOT use the cargo, git, gh, or docker tools directly — run all commands through exec(). Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].\""
    else
      CLAUDE_ARGS="$CLAUDE_ARGS --append-system-prompt \"Use daimonos MCP tools, not built-in equivalents. If your plan requires 2+ tool calls, use execute_script to run them as a single Starlark script — tool functions are already available (see server instructions for signatures). Only call individual tools for single-operation tasks. Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].\""
    fi
  elif [ "$MODE" = "baseline-terse" ]; then
    # Prompt-symmetry control arm (vikunja #925): baseline tools + the SAME
    # terse-style directive the daimonos arm carries, minus the tool-routing
    # sentences. daimonos-vs-baseline-terse isolates the tools-only effect;
    # ponytail's caveman control showed terse-prose alone moves tokens.
    CLAUDE_ARGS="$CLAUDE_ARGS --append-system-prompt \"Terse output. Drop filler, articles, pleasantries, hedging. Fragments OK. Technical substance exact. Code unchanged. Pattern: [thing] [action] [reason].\""
  fi

  start_s="$(date +%s)"

  cd "$WORKSPACE"
  # shellcheck disable=SC2086
  printf '%s' "$prompt" | eval "\"$CLAUDE\" $CLAUDE_ARGS" > "$raw_file" 2>/dev/null || true

  end_s="$(date +%s)"
  wall_ms=$(( (end_s - start_s) * 1000 ))

  # Parse results using node
  node -e "
var fs = require('fs');
var rawFile = process.argv[1];
var outFile = process.argv[2];
var taskId = process.argv[3];
var taskName = process.argv[4];
var mode = process.argv[5];
var wallMs = parseInt(process.argv[6]);

var events = [];
var lines = fs.readFileSync(rawFile, 'utf8').split('\\n');
for (var i = 0; i < lines.length; i++) {
  var line = lines[i].trim();
  if (line) {
    try { events.push(JSON.parse(line)); } catch(e) {}
  }
}

var resultEvent = null;
for (var i = 0; i < events.length; i++) {
  if (events[i].type === 'result') { resultEvent = events[i]; break; }
}

var usage = resultEvent ? (resultEvent.usage || {}) : {};
var durationMs = resultEvent ? (resultEvent.duration_ms || 0) : 0;
var durationApiMs = resultEvent ? (resultEvent.duration_api_ms || 0) : 0;
var costUsd = resultEvent ? (resultEvent.total_cost_usd || 0) : 0;
var numTurns = resultEvent ? (resultEvent.num_turns || 0) : 0;
var isError = resultEvent ? (resultEvent.is_error !== undefined ? resultEvent.is_error : true) : true;

var toolCalls = 0, mcpToolCalls = 0, builtinToolCalls = 0;
for (var i = 0; i < events.length; i++) {
  if (events[i].type === 'assistant') {
    var content = (events[i].message || {}).content || [];
    if (Array.isArray(content)) {
      for (var j = 0; j < content.length; j++) {
        if (content[j].type === 'tool_use') {
          toolCalls++;
          if ((content[j].name || '').indexOf('mcp__daimonos__') === 0) mcpToolCalls++;
          else builtinToolCalls++;
        }
      }
    }
  }
}

var inputTokens = usage.input_tokens || 0;
var outputTokens = usage.output_tokens || 0;

// Contamination canary (vikunja #926): a non-daimonos arm that reaches any
// mcp__daimonos__ tool means the isolation failed and the run's numbers lie.
var contaminated = (mode !== 'daimonos') && mcpToolCalls > 0;

var summary = {
  task_id: taskId, task_name: taskName, mode: mode,
  wall_ms: wallMs, duration_ms: durationMs, duration_api_ms: durationApiMs,
  cost_usd: costUsd, num_turns: numTurns,
  input_tokens: inputTokens, output_tokens: outputTokens,
  cache_read_tokens: usage.cache_read_input_tokens || 0,
  cache_write_tokens: usage.cache_creation_input_tokens || 0,
  total_tokens: inputTokens + outputTokens,
  tool_calls: toolCalls, mcp_tool_calls: mcpToolCalls,
  builtin_tool_calls: builtinToolCalls,
  is_error: isError, success: !isError && !contaminated,
  contaminated: contaminated
};

fs.writeFileSync(outFile, JSON.stringify(summary, null, 2));

if (contaminated) {
  console.log('       *** CONTAMINATED: ' + mode + ' arm made ' + mcpToolCalls +
    ' daimonos MCP call(s) — isolation failed, run marked invalid ***');
}

var tcDetail = mode === 'daimonos'
  ? 'mcp:' + mcpToolCalls + ' builtin:' + builtinToolCalls
  : '' + toolCalls;
console.log('       tokens: ' + summary.total_tokens.toLocaleString() +
  ' (in:' + inputTokens.toLocaleString() + ' out:' + outputTokens.toLocaleString() +
  ') | tools: ' + tcDetail + ' | cost: \$' + costUsd.toFixed(4) +
  ' | wall: ' + wallMs.toLocaleString() + 'ms');
" "$raw_file" "$out_file" "$task_id" "$task_name" "$MODE" "$wall_ms"
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
    RUN_ID="$(date +%Y%m%d-%H%M%S)-${MODE}-${effective_tag}"
  else
    RUN_ID="$(date +%Y%m%d-%H%M%S)-${MODE}"
  fi
  RUN_DIR="$RESULTS_DIR/$RUN_ID"
  mkdir -p "$RUN_DIR"

  echo "=== Benchmark Run ($run_number/$RUNS) ==="
  echo "Mode:      $MODE"
  echo "Model:     $MODEL"
  echo "Run ID:    $RUN_ID"
  echo "Workspace: $WORKSPACE"
  echo ""

  for task_file in "$TASKS_DIR"/*.json; do
    run_task "$task_file"
  done

  echo ""
  run_number=$((run_number + 1))
done

echo "=== All runs complete ==="
echo ""
echo "To analyze results, run:"
echo "  python3 $SCRIPT_DIR/analyze-results.py $RESULTS_DIR"
