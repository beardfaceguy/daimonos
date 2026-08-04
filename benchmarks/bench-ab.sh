#!/bin/sh
# bench-ab.sh - A/B the daimonos agent benchmark across two binaries.
#
# Runs the SAME task suite (benchmarks/bench-agent.sh) twice - once against a
# "before optimizations" baseline binary and once against a "post" binary - with
# distinct BENCH_TAGs, then prints an aggregated, correctness-gated comparison.
#
# Defaults line up with the optimization-lineage.json entries:
#   PRE  = bench/bin/daimonos-c3d103a  (F0 baseline, commit c3d103a / #121)
#   POST = bench/bin/daimonos-a31e470  (end of the #122-#126 round, commit a31e470 / #126)
#
# Model/provider/key come from the agent env file, exactly like bench-agent.sh
# (~/.config/daimonos/agent.env). This spends real API budget - smoke one task
# first:  ./bench-ab.sh 01
#
# Usage:
#   ./bench-ab.sh [task-id-prefix]
#
# Environment overrides:
#   PRE_BIN, POST_BIN    binary paths (defaults above)
#   PRE_TAG, POST_TAG    results-dir labels (default pre-opt-c3d103a / post-opt-a31e470)
#   SKIP_ANALYZE=1       run both suites but skip the analyze.py comparison
#
# POSIX sh compatible.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$SCRIPT_DIR/../bench/bin"

PRE_BIN="${PRE_BIN:-$BIN_DIR/daimonos-c3d103a}"
POST_BIN="${POST_BIN:-$BIN_DIR/daimonos-a31e470}"
PRE_TAG="${PRE_TAG:-pre-opt-c3d103a}"
POST_TAG="${POST_TAG:-post-opt-a31e470}"
TASK_FILTER="${1:-}"

for b in "$PRE_BIN" "$POST_BIN"; do
  [ -x "$b" ] || { echo "Error: binary not found/executable: $b"; echo "  (rebuild from the git_commit in benchmarks/optimization-lineage.json)"; exit 1; }
done

echo "=== A/B daimonos agent benchmark ==="
echo "PRE  ($PRE_TAG):  $PRE_BIN"
echo "POST ($POST_TAG): $POST_BIN"
echo "Task filter:      ${TASK_FILTER:-<all>}"
echo

echo "--- [1/2] PRE baseline ---"
DAIMONOS_BIN="$PRE_BIN"  BENCH_TAG="$PRE_TAG"  "$SCRIPT_DIR/bench-agent.sh" "$TASK_FILTER"
echo
echo "--- [2/2] POST ---"
DAIMONOS_BIN="$POST_BIN" BENCH_TAG="$POST_TAG" "$SCRIPT_DIR/bench-agent.sh" "$TASK_FILTER"

if [ "${SKIP_ANALYZE:-0}" != "1" ]; then
  echo
  echo "=== PRE aggregate ($PRE_TAG) ==="
  python3 "$SCRIPT_DIR/analyze.py" "$SCRIPT_DIR/results/" "$PRE_TAG"
  echo
  echo "=== POST aggregate ($POST_TAG) ==="
  python3 "$SCRIPT_DIR/analyze.py" "$SCRIPT_DIR/results/" "$POST_TAG"
  echo
  echo "Compare the two aggregate tables above (mean tokens / cost / correctness)."
fi
