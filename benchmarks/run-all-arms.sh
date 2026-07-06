#!/bin/sh
# Full three-arm gated benchmark (#178): baseline / baseline-terse / daimonos.
#
# Order of operations:
#   1. Fresh release build (benchmark measures current code, not a stale binary)
#   2. setup-mcp.sh (points workspace .cursor/mcp.json at the release binary)
#   3. Smoke gate: one cheap daimonos task; abort before the expensive arms
#      if the MCP wiring is broken (mcp_tool_calls must be > 0)
#   4. BENCH_RUNS x each arm, then the analyzer
#
# Usage: ./run-all-arms.sh [tag]     (default tag: gated)
# Env:   BENCH_RUNS (default 4), BENCH_MODEL (default opus)
#
# Writes progress to stdout; callers detaching it should redirect to a log.
set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TAG="${1:-gated}"
RUNS="${BENCH_RUNS:-4}"

cd "$SCRIPT_DIR/.." || exit 1

echo "=== three-arm benchmark: tag=$TAG runs=$RUNS ==="
echo "--- release build ---"
cargo build --release || { echo "RESULT: BUILD_FAIL"; exit 1; }

cd "$SCRIPT_DIR" || exit 1
./setup-mcp.sh || { echo "RESULT: SETUP_FAIL"; exit 1; }

echo "--- smoke gate (1 daimonos task) ---"
BENCH_TAG="${TAG}-smoke" ./run-benchmark.sh daimonos 01 || { echo "RESULT: SMOKE_RUN_FAIL"; exit 1; }
smoke_dir=$(ls -td results/*-daimonos-"${TAG}"-smoke 2>/dev/null | head -1)
python3 - "$smoke_dir/01-read-understand.json" <<'PYEOF' || { echo "RESULT: SMOKE_GATE_FAIL"; exit 1; }
import json, sys
s = json.load(open(sys.argv[1]))
ok = s.get("mcp_tool_calls", 0) > 0 and not s.get("is_error")
print(f"smoke: mcp_tool_calls={s.get('mcp_tool_calls')} is_error={s.get('is_error')} -> {'OK' if ok else 'FAIL'}")
sys.exit(0 if ok else 1)
PYEOF

status=ok
for arm in baseline baseline-terse daimonos; do
    echo "--- arm: $arm (n=$RUNS) ---"
    BENCH_RUNS="$RUNS" BENCH_TAG="$TAG" ./run-benchmark.sh "$arm" || status="arm_fail:$arm"
done

echo "--- analysis ---"
python3 analyze-results.py results/ "$TAG"

echo "RESULT: $status"
[ "$status" = "ok" ] || exit 1
