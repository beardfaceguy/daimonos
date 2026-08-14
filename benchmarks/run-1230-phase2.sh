#!/bin/sh
# vikunja #1230 Phase 2 re-baseline + read-transform-write intervention.
#
# Two arms, same commit (50b0c86), differing ONLY in prompts/agent_system.md:
#   base = current prompt
#   rtw  = adds the "you do not need to read a file to change it" idiom
#
# Task 03 is deterministic (5 calls across 6 prior runs) so n=3 suffices.
# Task 07 is noisy (4-7 calls) so it gets n=5. Task 02 is dropped: already at
# the 2-call floor, contributes only noise (PR #159 finding).
#
# Usage: ./run-1230-phase2.sh <task-id> <reps>
set -u
cd "$(dirname "$0")" || exit 1

TASK="${1:?task id prefix required}"
REPS="${2:?rep count required}"
SHA=50b0c86

for arm in base rtw; do
  bin="$PWD/bin/daimonos-$arm-$SHA"
  [ -x "$bin" ] || { echo "missing binary: $bin" >&2; exit 1; }
  i=1
  while [ "$i" -le "$REPS" ]; do
    echo "=== task $TASK | arm $arm | rep $i/$REPS ==="
    DAIMONOS_BIN="$bin" BENCH_TAG="1230p2-$arm-t$TASK-r$i" ./bench-agent.sh "$TASK"
    i=$((i + 1))
  done
done
echo "=== done: task $TASK, $REPS reps x 2 arms ==="
