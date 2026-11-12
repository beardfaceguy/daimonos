"""ACP permission requests must not park a turn forever (vikunja #1616).

A Zed session hung for 27+ minutes because daimonos sent
session/request_permission for a remote MCP tool and the client never answered.
These tests drive the real binary with a mock OpenRouter endpoint and a local
stdio MCP server; no network calls leave 127.0.0.1.
"""
import json
import queue
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

MCP_SERVER = r'''
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "probe", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "get_self", "description": "Return a fixed identity.",
                             "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "PROBE_SELF_OK"}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)
'''


class MockProvider:
    """OpenRouter-compatible SSE: first call requests get_self, later calls finish."""

    def __init__(self):
        self.tool_results = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{"id": "test/model", "context_length": 100000,
                    "supported_parameters": ["tools"], "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                names = [tool["function"]["name"] for tool in body.get("tools", [])]
                target = next((name for name in names if name.endswith("get_self")), None)
                results = [m for m in body["messages"] if m["role"] == "tool"]
                owner.tool_results.extend(json.dumps(m.get("content")) for m in results)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                if target and not results:
                    delta = {"role": "assistant", "content": "", "tool_calls": [{"index": 0, "id": "call-1",
                             "type": "function", "function": {"name": target, "arguments": "{}"}}]}
                    reason = "tool_calls"
                else:
                    delta, reason = {"role": "assistant", "content": "DONE"}, "stop"
                chunk = {"choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                         "usage": {"prompt_tokens": 1, "completion_tokens": 1}}
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode())
                self.wfile.flush()

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class AcpClient:
    def __init__(self, binary, tmp_path, port, agent_env_extra=(), config_extra=""):
        self.workspace = tmp_path / "workspace"
        self.workspace.mkdir()
        home = tmp_path / "home"
        home.mkdir()
        self.log_dir = tmp_path / "logs"
        config = tmp_path / "config.toml"
        config.write_text(
            "[analytics]\nenabled=false\n"
            f'[logging]\nlevel="info"\ndirectory="{self.log_dir}"\n' + config_extra + "\n"
        )
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{port}", "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_APPROVAL_MODE=interactive", "DAIMONOS_AGENT_COMPACTION=off",
            *agent_env_extra, ""]))
        self.mcp_script = tmp_path / "mcp_server.py"
        self.mcp_script.write_text(MCP_SERVER)
        self.permission_requests = []
        self.responses = queue.Queue()
        self.process = subprocess.Popen(
            [binary, "--workspace", str(self.workspace), "--config", str(config),
             "acp", "--agent-env", str(agent_env)],
            cwd=self.workspace, env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, bufsize=1)
        threading.Thread(target=self._read, daemon=True).start()
        self.next_id = 0

    def _read(self):
        for line in self.process.stdout:
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("method") == "session/request_permission":
                self.permission_requests.append(msg)  # deliberately never answered
            elif "id" in msg and "method" not in msg:
                self.responses.put(msg)
            elif "id" in msg:
                self._send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}})

    def _send(self, value):
        self.process.stdin.write(json.dumps(value) + "\n")
        self.process.stdin.flush()

    def request(self, method, params, timeout):
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            msg = self.responses.get(timeout=max(0.01, deadline - time.monotonic()))
            if msg["id"] == self.next_id:
                return msg

    def log_text(self):
        return "".join(path.read_text() for path in sorted(self.log_dir.glob("*")))

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


@pytest.fixture
def provider():
    mock = MockProvider()
    yield mock
    mock.close()


def open_session(client):
    client.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}}, 30)
    servers = [{"name": "probe", "command": sys.executable, "args": ["-I", str(client.mcp_script)], "env": []}]
    new = client.request("session/new", {"cwd": str(client.workspace), "mcpServers": servers}, 60)
    return new["result"]["sessionId"]


def test_unanswered_permission_is_denied_after_timeout(daimonos_binary, tmp_path, provider):
    client = AcpClient(daimonos_binary, tmp_path, provider.server.server_port,
                       config_extra="[acp]\napproval_timeout_secs=2")
    try:
        session_id = open_session(client)
        started = time.monotonic()
        reply = client.request("session/prompt", {"sessionId": session_id,
                               "prompt": [{"type": "text", "text": "call get_self"}]}, 30)
        elapsed = time.monotonic() - started
    finally:
        client.close()

    assert reply["result"]["stopReason"] == "end_turn"
    assert len(client.permission_requests) == 1
    assert 2 <= elapsed < 15
    assert any("timed out" in result for result in provider.tool_results), provider.tool_results
    logs = client.log_text()
    assert '"event":"approval_requested"' in logs
    assert '"resolved_by":"approval_response_timeout"' in logs


def run_one_prompt(client):
    session_id = open_session(client)
    return client.request("session/prompt", {"sessionId": session_id,
                          "prompt": [{"type": "text", "text": "call get_self"}]}, 30)


def test_allow_list_runs_listed_remote_tool_without_asking(daimonos_binary, tmp_path, provider):
    client = AcpClient(daimonos_binary, tmp_path, provider.server.server_port, agent_env_extra=[
        "DAIMONOS_AGENT_PERMISSION_MODE=allow_list", "DAIMONOS_AGENT_PERMISSION_LIST=probe/get_self"])
    try:
        reply = run_one_prompt(client)
    finally:
        client.close()
    assert reply["result"]["stopReason"] == "end_turn"
    assert client.permission_requests == []
    assert any("PROBE_SELF_OK" in result for result in provider.tool_results), provider.tool_results


def test_deny_list_asks_for_listed_remote_tool(daimonos_binary, tmp_path, provider):
    client = AcpClient(daimonos_binary, tmp_path, provider.server.server_port,
                       agent_env_extra=["DAIMONOS_AGENT_PERMISSION_MODE=deny_list",
                                        "DAIMONOS_AGENT_PERMISSION_LIST=probe/*"],
                       config_extra="[acp]\napproval_timeout_secs=2")
    try:
        reply = run_one_prompt(client)
    finally:
        client.close()
    assert reply["result"]["stopReason"] == "end_turn"
    assert len(client.permission_requests) == 1
