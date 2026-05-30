from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from typing import Dict, List, Optional

import pytest


class DaimonosClient:
    """Manages a daimonos subprocess and sends JSON-RPC over stdio."""

    def __init__(self, process, workspace):
        self.process = process
        self.workspace = workspace
        self._id = 0

    def _next_id(self):
        self._id += 1
        return self._id

    def send_raw(self, msg: dict) -> Optional[dict]:
        """Send a JSON-RPC message. Returns the response, or None for notifications."""
        line = json.dumps(msg) + "\n"
        self.process.stdin.write(line.encode())
        self.process.stdin.flush()
        if "id" not in msg:
            return None
        resp_line = self.process.stdout.readline()
        if not resp_line:
            raise RuntimeError("daimonos process closed stdout unexpectedly")
        return json.loads(resp_line)

    def call_tool(self, name: str, arguments: Optional[dict] = None) -> dict:
        req = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments or {}},
        }
        resp = self.send_raw(req)
        if "error" in resp:
            raise RuntimeError(f"RPC error: {resp['error']}")
        return resp.get("result", {})

    def list_tools(self) -> List[dict]:
        req = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": "tools/list",
            "params": {},
        }
        resp = self.send_raw(req)
        if "error" in resp:
            raise RuntimeError(f"RPC error: {resp['error']}")
        return resp.get("result", {}).get("tools", [])


def _find_binary():
    """Return path to daimonos binary, building if necessary."""
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    binary = os.path.join(repo_root, "target", "debug", "daimonos")
    return binary, repo_root


@pytest.fixture(scope="session")
def daimonos_binary():
    """Build the daimonos binary once per test session."""
    binary, repo_root = _find_binary()
    result = subprocess.run(
        ["cargo", "build"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if result.returncode != 0:
        pytest.fail(f"cargo build failed:\n{result.stderr}")
    assert os.path.isfile(binary), f"binary not found at {binary}"
    return binary


@pytest.fixture
def daimonos(daimonos_binary, tmp_path):
    """
    Spawn a daimonos MCP subprocess, perform the handshake,
    and yield a DaimonosClient. Tears down on cleanup.
    """
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(tmp_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    client = DaimonosClient(proc, str(tmp_path))

    # MCP handshake: initialize
    init_resp = client.send_raw({
        "jsonrpc": "2.0",
        "id": client._next_id(),
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1.0.0"},
        },
    })
    assert "result" in init_resp, f"initialize failed: {init_resp}"

    # MCP handshake: initialized notification
    client.send_raw({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    })

    yield client

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


@pytest.fixture
def daimonos_observe(daimonos_binary, tmp_path):
    """Like `daimonos`, but with KGL observed-provenance capture enabled
    (DAIMONOS_KGL_OBSERVE=1)."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(tmp_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ, "DAIMONOS_KGL_OBSERVE": "1"},
    )
    client = DaimonosClient(proc, str(tmp_path))
    init_resp = client.send_raw({
        "jsonrpc": "2.0",
        "id": client._next_id(),
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1.0.0"},
        },
    })
    assert "result" in init_resp, f"initialize failed: {init_resp}"
    client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})

    yield client

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
