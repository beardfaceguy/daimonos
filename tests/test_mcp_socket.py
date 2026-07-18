"""Phase 1 (#240): MCP-over-socket — handshake, tool calls, parallel session isolation."""

import json
import os
import socket
import subprocess
import threading
import time

import pytest


class McpSocketClient:
    """Raw MCP JSON-RPC client over a Unix domain socket."""

    def __init__(self, sock_path: str):
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.connect(sock_path)
        self._rfile = self._sock.makefile("rb")
        self._id = 0
        self.notifications = []

    def close(self):
        self._sock.close()

    def _next_id(self):
        self._id += 1
        return self._id

    def send_raw(self, msg: dict):
        line = json.dumps(msg) + "\n"
        self._sock.sendall(line.encode())
        if "id" not in msg:
            return None
        request_id = msg["id"]
        while True:
            resp_line = self._rfile.readline()
            if not resp_line:
                raise RuntimeError("server closed connection")
            response = json.loads(resp_line)
            if response.get("id") == request_id:
                return response
            self.notifications.append(response)

    def handshake(self, roots=None):
        capabilities = {}
        if roots is not None:
            capabilities["roots"] = {"listChanged": True}
        resp = self.send_raw({
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": capabilities,
                "clientInfo": {"name": "pytest-socket", "version": "0.1"},
            },
        })
        assert "result" in resp, f"initialize failed: {resp}"
        self.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})
        if roots is not None:
            # The server must ask for roots after initialized. Reply before
            # sending a ping barrier so the server applies the re-root before
            # handshake() returns.
            raw = self._rfile.readline()
            if not raw:
                raise RuntimeError("server closed before roots/list")
            request = json.loads(raw)
            assert request.get("method") == "roots/list", request
            self._sock.sendall(
                (
                    json.dumps({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"roots": roots},
                    })
                    + "\n"
                ).encode()
            )
            ping = self.send_raw({
                "jsonrpc": "2.0",
                "id": self._next_id(),
                "method": "ping",
                "params": {},
            })
            assert "result" in ping

    def list_tools(self):
        resp = self.send_raw({
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "tools/list",
            "params": {},
        })
        if "error" in resp:
            raise RuntimeError(f"tools/list error: {resp['error']}")
        return resp.get("result", {}).get("tools", [])

    def call_tool(self, name: str, arguments: dict = None):
        resp = self.send_raw({
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments or {}},
        })
        if "error" in resp:
            raise RuntimeError(f"tools/call RPC error: {resp['error']}")
        return resp.get("result", {})


@pytest.fixture
def mcp_socket_server(daimonos_binary, tmp_path):
    """Start daimonos in --mcp-socket mode; yield (sock_path, workspace)."""
    sock_path = str(tmp_path / "mcp.sock")
    workspace = str(tmp_path / "ws")
    os.makedirs(workspace, exist_ok=True)

    proc = subprocess.Popen(
        [daimonos_binary, "--mcp-socket", sock_path, "-w", workspace],
        stderr=subprocess.PIPE,
    )

    for _ in range(100):
        if os.path.exists(sock_path):
            break
        time.sleep(0.05)
    else:
        proc.terminate()
        pytest.fail("--mcp-socket: socket file never appeared")

    yield sock_path, workspace, proc

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_mcp_socket_handshake(mcp_socket_server):
    """Client can complete MCP initialize handshake."""
    sock_path, _, _ = mcp_socket_server
    client = McpSocketClient(sock_path)
    try:
        client.handshake()
    finally:
        client.close()


def test_mcp_socket_initialize_result_shape(mcp_socket_server):
    """initialize result contains protocolVersion and serverInfo."""
    sock_path, _, _ = mcp_socket_server
    client = McpSocketClient(sock_path)
    try:
        resp = client.send_raw({
            "jsonrpc": "2.0",
            "id": client._next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"},
            },
        })
        result = resp.get("result", {})
        assert "protocolVersion" in result, f"missing protocolVersion: {result}"
        assert "serverInfo" in result, f"missing serverInfo: {result}"
        assert result["serverInfo"]["name"] == "daimonos"
    finally:
        client.close()


def test_mcp_socket_tools_list(mcp_socket_server):
    """tools/list returns at least read_file and write_file."""
    sock_path, _, _ = mcp_socket_server
    client = McpSocketClient(sock_path)
    try:
        client.handshake()
        tools = client.list_tools()
        names = [t["name"] for t in tools]
        assert "read_file" in names, f"read_file missing; got: {names}"
        assert "write_file" in names, f"write_file missing; got: {names}"
    finally:
        client.close()


def test_mcp_socket_call_read_file(mcp_socket_server):
    """tools/call read_file returns the file's content."""
    sock_path, workspace, _ = mcp_socket_server
    test_file = os.path.join(workspace, "hello.txt")
    with open(test_file, "w") as f:
        f.write("hello socket mcp\n")

    client = McpSocketClient(sock_path)
    try:
        client.handshake()
        result = client.call_tool("read_file", {"path": test_file})
        text = result["content"][0]["text"]
        assert "hello socket mcp" in text, f"unexpected content: {text}"
    finally:
        client.close()


def test_mcp_socket_call_write_file(mcp_socket_server, tmp_path):
    """tools/call write_file creates a file on disk."""
    sock_path, workspace, _ = mcp_socket_server
    target = os.path.join(workspace, "out.txt")

    client = McpSocketClient(sock_path)
    try:
        client.handshake()
        client.call_tool("write_file", {"path": target, "content": "written via socket\n"})
        assert os.path.exists(target), "write_file did not create the file"
        with open(target) as f:
            assert "written via socket" in f.read()
    finally:
        client.close()


def test_mcp_socket_roots_reroot_workspace(mcp_socket_server, tmp_path):
    """A roots-capable socket client can replace the launch workspace."""
    sock_path, launch_workspace, _ = mcp_socket_server
    project = tmp_path / "real_project"
    project.mkdir()
    root_uri = f"file://{os.path.realpath(project)}"

    client = McpSocketClient(sock_path)
    try:
        client.handshake(roots=[{"uri": root_uri, "name": "project"}])
        result = client.call_tool("workspace_info")
        payload = json.loads(result["content"][0]["text"])
        assert payload["session"]["workspace"] == os.path.realpath(project)
        assert payload["session"]["workspace"] != os.path.realpath(launch_workspace)
    finally:
        client.close()


def test_mcp_socket_execute_script(mcp_socket_server):
    """execute_script is callable over socket, matching stdio MCP."""
    sock_path, workspace, _ = mcp_socket_server
    target = os.path.join(workspace, "from_script.txt")

    client = McpSocketClient(sock_path)
    try:
        client.handshake()
        result = client.call_tool(
            "execute_script",
            {
                "code": (
                    "write_file('from_script.txt', 'written by socket script')\n"
                    "result = 'ok'"
                )
            },
        )
        assert result.get("isError") is not True, result
        assert os.path.exists(target)
        with open(target) as file:
            assert file.read() == "written by socket script"
    finally:
        client.close()


def test_mcp_socket_parallel_sessions_isolated(mcp_socket_server):
    """Two concurrent sessions must not share read-cache or cwd state."""
    sock_path, workspace, _ = mcp_socket_server

    ws_a = os.path.join(workspace, "client_a")
    ws_b = os.path.join(workspace, "client_b")
    os.makedirs(ws_a, exist_ok=True)
    os.makedirs(ws_b, exist_ok=True)

    with open(os.path.join(ws_a, "file.txt"), "w") as f:
        f.write("from workspace A\n")
    with open(os.path.join(ws_b, "file.txt"), "w") as f:
        f.write("from workspace B\n")

    results: dict = {}
    errors: list = []

    def run_client(label: str, file_path: str):
        try:
            c = McpSocketClient(sock_path)
            c.handshake()
            result = c.call_tool("read_file", {"path": file_path})
            results[label] = result["content"][0]["text"]
            c.close()
        except Exception as e:
            errors.append(f"{label}: {e}")

    t_a = threading.Thread(target=run_client, args=("a", os.path.join(ws_a, "file.txt")))
    t_b = threading.Thread(target=run_client, args=("b", os.path.join(ws_b, "file.txt")))
    t_a.start()
    t_b.start()
    t_a.join(timeout=30)
    t_b.join(timeout=30)

    assert not errors, f"client errors: {errors}"
    assert "from workspace A" in results.get("a", ""), f"client A got: {results.get('a')}"
    assert "from workspace B" in results.get("b", ""), f"client B got: {results.get('b')}"
    assert "from workspace B" not in results.get("a", ""), "session cross-contamination: A got B's data"
    assert "from workspace A" not in results.get("b", ""), "session cross-contamination: B got A's data"


def test_mcp_socket_multiple_sequential_connections(mcp_socket_server):
    """Server handles multiple sequential connections without state leakage."""
    sock_path, workspace, _ = mcp_socket_server

    for i in range(3):
        fname = os.path.join(workspace, f"seq_{i}.txt")
        with open(fname, "w") as f:
            f.write(f"content {i}\n")

        client = McpSocketClient(sock_path)
        client.handshake()
        result = client.call_tool("read_file", {"path": fname})
        text = result["content"][0]["text"]
        client.close()

        assert f"content {i}" in text, f"conn {i}: unexpected content: {text}"


def test_mcp_socket_ping(mcp_socket_server):
    """ping method returns an empty result."""
    sock_path, _, _ = mcp_socket_server
    client = McpSocketClient(sock_path)
    try:
        client.handshake()
        resp = client.send_raw({
            "jsonrpc": "2.0",
            "id": client._next_id(),
            "method": "ping",
            "params": {},
        })
        assert "result" in resp, f"ping should return a result; got: {resp}"
    finally:
        client.close()
