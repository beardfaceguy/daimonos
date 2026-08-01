#!/bin/sh
# Deterministic, API-free A/B benchmark for Vikunja 1193/1194.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RESULTS_DIR="$SCRIPT_DIR/results"
RUN_TAG="${BENCH_TAG:-}"

cd "$REPO_ROOT"
output="$(cargo test controlled_ -- --ignored --nocapture 2>&1)"
printf '%s\n' "$output"

agent_metrics="$(printf '%s\n' "$output" | awk '
  /^CONTROLLED_TOOL_OUTPUT_BENCH=/ {
    sub(/^CONTROLLED_TOOL_OUTPUT_BENCH=/, "")
    print
    exit
  }
')"
mcp_metrics="$(printf '%s\n' "$output" | awk '
  /^CONTROLLED_MCP_OUTPUT_BENCH=/ {
    sub(/^CONTROLLED_MCP_OUTPUT_BENCH=/, "")
    print
    exit
  }
')"
[ -n "$agent_metrics" ] && [ -n "$mcp_metrics" ] || {
  echo "Error: controlled benchmark emitted incomplete metrics" >&2
  exit 1
}

mkdir -p "$RESULTS_DIR"
run_id="$(date +%Y%m%d-%H%M%S)-controlled-tool-output${RUN_TAG:+-$RUN_TAG}"
artifact="$RESULTS_DIR/$run_id.json"
created_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
git_commit="$(git rev-parse HEAD)"

python3 -c '
import json
import sys

artifact = {
    "agent": json.loads(sys.argv[1]),
    "mcp": json.loads(sys.argv[2]),
    "created_at": sys.argv[4],
    "git_commit": sys.argv[5],
}
with open(sys.argv[3], "w", encoding="utf-8") as handle:
    json.dump(artifact, handle, indent=2, sort_keys=True)
    handle.write("\n")
' "$agent_metrics" "$mcp_metrics" "$artifact" "$created_at" "$git_commit"

echo
echo "Artifact: $artifact"
sha256sum "$artifact"
