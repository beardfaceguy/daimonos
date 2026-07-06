#!/bin/bash
#
# MCP smoke test for daimonos — validates the server responds correctly
# over SSH (remote) or stdio (local).
#
# Usage:
#   ./smoke-test.sh local /path/to/daimonos       # test binary directly via stdio
#   ./smoke-test.sh ssh   agent@host [-i key]      # test via SSH
#   ./smoke-test.sh qemu  [port]                   # test QEMU instance on localhost
#
set -euo pipefail

MODE="${1:-}"
FAILURES=0
TESTS=0

die()  { echo "FAIL: $*" >&2; exit 1; }
pass() { TESTS=$((TESTS + 1)); echo "  PASS: $1"; }
fail() { TESTS=$((TESTS + 1)); FAILURES=$((FAILURES + 1)); echo "  FAIL: $1"; }

mcp_session() {
    local init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-test","version":"1.0"}}}'
    local notif='{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'

    local tools_list='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
    local exec_uname='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"exec","arguments":{"command":"uname","args":["-s"]}}}'
    local ls_root='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ls","arguments":{"path":"/"}}}'
    local write_test='{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"smoke-test.txt","content":"hello from smoke test"}}}'
    local read_test='{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"smoke-test.txt"}}}'

    local out_file
    out_file=$(mktemp)

    # Response-driven pacing: after sending each request, wait (bounded) for
    # its response to land in $out_file before sending the next / closing
    # stdin. The previous fixed-sleep version closed stdin 3s after the last
    # request, which truncated the session on slow CI runners (flaked on
    # run 28815096979: read_file response missed the window).
    (
        send_and_wait() {
            local req="$1" id="$2" i=0
            echo "$req"
            while [ "$i" -lt 120 ] && ! grep -q "\"id\":${id}," "$out_file" 2>/dev/null; do
                sleep 0.25
                i=$((i + 1))
            done
        }
        send_and_wait "$init" 1
        echo "$notif"
        send_and_wait "$tools_list" 2
        send_and_wait "$exec_uname" 3
        send_and_wait "$ls_root" 4
        send_and_wait "$write_test" 5
        send_and_wait "$read_test" 6
    ) | eval "$MCP_CMD" >"$out_file" 2>/dev/null

    cat "$out_file"
    rm -f "$out_file"
}

check_response() {
    local id="$1" label="$2" pattern="$3" responses="$4"
    local line
    line=$(echo "$responses" | grep "\"id\":${id}," || true)
    if [ -z "$line" ]; then
        fail "$label — no response for id=$id"
        return
    fi
    if echo "$line" | grep -q '"error"'; then
        fail "$label — got error: $(echo "$line" | grep -o '"message":"[^"]*"')"
        return
    fi
    if echo "$line" | grep -q "$pattern"; then
        pass "$label"
    else
        fail "$label — response didn't match pattern '$pattern'"
    fi
}

case "$MODE" in
    local)
        BINARY="${2:?Usage: smoke-test.sh local /path/to/daimonos}"
        [ -x "$BINARY" ] || die "Binary not found or not executable: $BINARY"
        WORKDIR=$(mktemp -d)
        trap 'rm -rf "$WORKDIR"' EXIT
        MCP_CMD="$BINARY --mcp -w $WORKDIR"
        ;;
    ssh)
        TARGET="${2:?Usage: smoke-test.sh ssh agent@host [-i key]}"
        shift 2
        MCP_CMD="ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 $* $TARGET"
        ;;
    qemu)
        PORT="${2:-2222}"
        MCP_CMD="ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 -p $PORT -i ~/.ssh/id_ed25519 agent@localhost"
        ;;
    *)
        echo "Usage: $0 {local|ssh|qemu} [args...]"
        echo ""
        echo "  local /path/to/binary    Test binary directly via stdio"
        echo "  ssh   agent@host [opts]  Test via SSH"
        echo "  qemu  [port]            Test QEMU on localhost (default port 2222)"
        exit 1
        ;;
esac

echo "=== daimonos MCP smoke test ==="
echo "Mode: $MODE"
echo ""

RESPONSES=$(mcp_session)

echo "Tests:"

check_response 1 "initialize handshake" 'protocolVersion' "$RESPONSES"
check_response 2 "tools/list returns tools" 'tools' "$RESPONSES"
check_response 3 "exec uname" 'Linux' "$RESPONSES"
check_response 4 "ls /" 'entries' "$RESPONSES"
check_response 5 "write_file" 'content' "$RESPONSES"
check_response 6 "read_file smoke-test.txt" 'hello from smoke test' "$RESPONSES"

echo ""
echo "Results: $((TESTS - FAILURES))/$TESTS passed"

if [ "$FAILURES" -gt 0 ]; then
    echo "FAIL"
    exit 1
else
    echo "PASS"
    exit 0
fi
