#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$SCRIPT_DIR/workspace"
DAIMONOS_BIN="${DAIMONOS_BIN:-$SCRIPT_DIR/../target/release/daimonos}"

if [[ ! -f "$DAIMONOS_BIN" ]]; then
  echo "Building daimonos (release)..."
  cargo build --release --manifest-path "$SCRIPT_DIR/../Cargo.toml"
fi

DAIMONOS_ABS="$(realpath "$DAIMONOS_BIN")"
WORKSPACE_ABS="$(realpath "$WORKSPACE")"

mkdir -p "$WORKSPACE/.cursor"

cat > "$WORKSPACE/.cursor/mcp.json" <<EOF
{
  "mcpServers": {
    "daimonos": {
      "command": "$DAIMONOS_ABS",
      "args": ["--mcp", "-w", "$WORKSPACE_ABS"]
    }
  }
}
EOF

echo "MCP config written to $WORKSPACE/.cursor/mcp.json"
echo "  Binary:    $DAIMONOS_ABS"
echo "  Workspace: $WORKSPACE_ABS"
echo ""
echo "You can now run: ./run-benchmark.sh daimonos"
