#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
TASKS_DIR="$SCRIPT_DIR/tasks"
RESULTS_DIR="$SCRIPT_DIR/results"
CLAUDE="${CLAUDE_BIN:-$HOME/.local/bin/claude}"
MODEL="${BENCH_MODEL:-opus}"
DAIMONOS_BIN="${DAIMONOS_BIN:-$SCRIPT_DIR/../target/release/daimonos}"
MCP_CONFIG="$WORKSPACE/.cursor/mcp.json"

# Source API key if not already set
if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  API_KEY_FILE="$SCRIPT_DIR/../claude_api_key.env"
  if [[ -f "$API_KEY_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$API_KEY_FILE"
    export ANTHROPIC_API_KEY
  else
    echo "Error: ANTHROPIC_API_KEY not set and $API_KEY_FILE not found"
    exit 1
  fi
fi

MODE="${1:-}" # "baseline" or "daimonos"
TASK_FILTER="${2:-}" # optional: task id prefix to run a single task
RUN_TAG="${BENCH_TAG:-}" # optional tag for naming runs (e.g. model name)

if [[ -z "$MODE" ]]; then
  echo "Usage: $0 <baseline|daimonos> [task-id]"
  echo ""
  echo "Environment variables:"
  echo "  BENCH_MODEL     Model alias or slug (default: opus)"
  echo "  CLAUDE_BIN      Path to claude CLI (default: ~/.local/bin/claude)"
  echo "  DAIMONOS_BIN    Path to daimonos binary (default: ../target/release/daimonos)"
  exit 1
fi

if [[ "$MODE" != "baseline" && "$MODE" != "daimonos" ]]; then
  echo "Error: mode must be 'baseline' or 'daimonos'"
  exit 1
fi

if [[ -n "$RUN_TAG" ]]; then
  RUN_ID="$(date +%Y%m%d-%H%M%S)-${MODE}-${RUN_TAG}"
else
  RUN_ID="$(date +%Y%m%d-%H%M%S)-${MODE}"
fi
RUN_DIR="$RESULTS_DIR/$RUN_ID"
mkdir -p "$RUN_DIR"

echo "=== Benchmark Run ==="
echo "Mode:      $MODE"
echo "Model:     $MODEL"
echo "Run ID:    $RUN_ID"
echo "Workspace: $WORKSPACE"
echo ""

reset_workspace() {
  cd "$WORKSPACE"
  git checkout -- . 2>/dev/null || true
  git clean -fd -e .cursor/ 2>/dev/null || true
}

run_task() {
  local task_file="$1"
  local task_id
  task_id="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['id'])" "$task_file")"
  local task_name
  task_name="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['name'])" "$task_file")"
  local applies_to
  applies_to="$(python3 -c "import json,sys; print(','.join(json.load(open(sys.argv[1]))['applies_to']))" "$task_file")"

  if [[ -n "$TASK_FILTER" && "$task_id" != "$TASK_FILTER"* ]]; then
    return 0
  fi

  local check_mode="$MODE"
  [[ "$check_mode" == "baseline" ]] && check_mode="cursor"
  if [[ "$applies_to" != *"$check_mode"* ]]; then
    echo "  SKIP $task_id ($task_name) — not applicable to $MODE"
    return 0
  fi

  local prompt
  prompt="$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['prompt'])" "$task_file")"

  echo "  RUN  $task_id: $task_name"
  reset_workspace

  local out_file="$RUN_DIR/${task_id}.json"

  local claude_args=(-p --output-format stream-json --verbose --model "$MODEL"
    --dangerously-skip-permissions --no-session-persistence --bare)

  if [[ "$MODE" == "daimonos" ]]; then
    claude_args+=(--mcp-config "$MCP_CONFIG" --strict-mcp-config)
    claude_args+=(--append-system-prompt "Use daimonos MCP tools, not built-in equivalents. If your plan requires 2+ tool calls, use execute_script to run them as a single Starlark script — tool functions are already available (see server instructions for signatures). Only call individual tools for single-operation tasks.")
  fi

  local start_ns
  start_ns="$(date +%s%N)"

  cd "$WORKSPACE"
  printf '%s' "$prompt" | "$CLAUDE" "${claude_args[@]}" > "$RUN_DIR/${task_id}.raw.jsonl" 2>/dev/null || true

  local end_ns
  end_ns="$(date +%s%N)"
  local wall_ms=$(( (end_ns - start_ns) / 1000000 ))

  python3 -c "
import json, sys

raw_file = sys.argv[1]
out_file = sys.argv[2]
task_id = sys.argv[3]
task_name = sys.argv[4]
mode = sys.argv[5]
wall_ms = int(sys.argv[6])

events = []
with open(raw_file) as f:
    for line in f:
        line = line.strip()
        if line:
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass

result_event = None
for e in events:
    if e.get('type') == 'result':
        result_event = e
        break

usage = result_event.get('usage', {}) if result_event else {}
duration_ms = result_event.get('duration_ms', 0) if result_event else 0
duration_api_ms = result_event.get('duration_api_ms', 0) if result_event else 0
cost_usd = result_event.get('total_cost_usd', 0) if result_event else 0
num_turns = result_event.get('num_turns', 0) if result_event else 0
is_error = result_event.get('is_error', True) if result_event else True

# Count tool calls: each assistant message content block with type 'tool_use'
tool_calls = 0
mcp_tool_calls = 0
builtin_tool_calls = 0

for e in events:
    if e.get('type') == 'assistant':
        msg = e.get('message', {})
        content = msg.get('content', [])
        if isinstance(content, list):
            for block in content:
                if block.get('type') == 'tool_use':
                    tool_calls += 1
                    tool_name = block.get('name', '')
                    if tool_name.startswith('mcp__daimonos__'):
                        mcp_tool_calls += 1
                    else:
                        builtin_tool_calls += 1

summary = {
    'task_id': task_id,
    'task_name': task_name,
    'mode': mode,
    'wall_ms': wall_ms,
    'duration_ms': duration_ms,
    'duration_api_ms': duration_api_ms,
    'cost_usd': cost_usd,
    'num_turns': num_turns,
    'input_tokens': usage.get('input_tokens', 0),
    'output_tokens': usage.get('output_tokens', 0),
    'cache_read_tokens': usage.get('cache_read_input_tokens', 0),
    'cache_write_tokens': usage.get('cache_creation_input_tokens', 0),
    'total_tokens': usage.get('input_tokens', 0) + usage.get('output_tokens', 0),
    'tool_calls': tool_calls,
    'mcp_tool_calls': mcp_tool_calls,
    'builtin_tool_calls': builtin_tool_calls,
    'is_error': is_error,
    'success': not is_error,
}

with open(out_file, 'w') as f:
    json.dump(summary, f, indent=2)

tc_detail = f'mcp:{mcp_tool_calls} builtin:{builtin_tool_calls}' if mode == 'daimonos' else f'{tool_calls}'
print(f'       tokens: {summary[\"total_tokens\"]:,} (in:{summary[\"input_tokens\"]:,} out:{summary[\"output_tokens\"]:,}) | tools: {tc_detail} | cost: \${cost_usd:.4f} | wall: {wall_ms:,}ms')
" "$RUN_DIR/${task_id}.raw.jsonl" "$out_file" "$task_id" "$task_name" "$MODE" "$wall_ms"
}

for task_file in "$TASKS_DIR"/*.json; do
  run_task "$task_file"
done

echo ""
echo "=== Run complete ==="
echo "Raw output: $RUN_DIR/"
echo ""
echo "To analyze results, run:"
echo "  python3 $SCRIPT_DIR/analyze-results.py $RESULTS_DIR"
