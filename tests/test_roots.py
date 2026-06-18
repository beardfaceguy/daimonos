"""End-to-end tests for MCP `roots` support (vikunja #46).

When a client advertises the `roots` capability, daimonos asks for the
client's workspace roots after initialization and re-roots the session onto
the first valid one — instead of staying pinned to whatever `-w`/cwd the
launcher hardcoded. Clients that don't support roots keep the launch
workspace (covered by the default `daimonos` fixture in conftest.py).

The stdio MCP handshake in conftest.py is strictly request -> single
response line, which can't cope with a server-initiated `roots/list`
request arriving mid-stream. These tests therefore drive the subprocess
directly with a background reader thread that (a) routes JSON-RPC responses
back to callers and (b) answers the server's `roots/list` request.
"""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time

import pytest


class RootsAwareClient:
    """A minimal MCP client that can answer server-initiated requests.

    A background thread reads every line from the subprocess. Lines that
    carry a `result`/`error` for one of our outbound ids are handed to the
    waiting caller; an inbound `roots/list` request is answered with the
    configured roots.
    """

    def __init__(self, process, roots):
        self.process = process
        self.roots = roots
        self._id = 0
        self._responses: "queue.Queue[dict]" = queue.Queue()
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self):
        for raw in self.process.stdout:
            line = raw.decode().strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            method = msg.get("method")
            if method == "roots/list":
                # Server -> client request: reply with our advertised roots.
                self._send({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {"roots": self.roots},
                })
            elif method is None:
                # A response to one of our requests.
                self._responses.put(msg)
            # Any other server-initiated request/notification is ignored.

    def _send(self, msg: dict):
        self.process.stdin.write((json.dumps(msg) + "\n").encode())
        self.process.stdin.flush()

    def _next_id(self):
        self._id += 1
        return self._id

    def request(self, method: str, params: dict) -> dict:
        rid = self._next_id()
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        # Drain until we see the matching id (responses arrive in order, but
        # be defensive in case of interleaving).
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                msg = self._responses.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if msg.get("id") == rid:
                return msg
        raise RuntimeError(f"no response for {method} (id={rid})")

    def notify(self, method: str):
        self._send({"jsonrpc": "2.0", "method": method})

    def call_tool(self, name: str, arguments: dict | None = None) -> dict:
        resp = self.request(
            "tools/call", {"name": name, "arguments": arguments or {}}
        )
        if "error" in resp:
            raise RuntimeError(f"RPC error: {resp['error']}")
        return resp.get("result", {})


def _spawn(binary, launch_ws, roots):
    proc = subprocess.Popen(
        [binary, "--mcp", "-w", str(launch_ws)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    client = RootsAwareClient(proc, roots)
    init = client.request(
        "initialize",
        {
            "protocolVersion": "2025-11-25",
            # Declaring the roots capability is what makes daimonos call back.
            "capabilities": {"roots": {"listChanged": True}},
            "clientInfo": {"name": "pytest-roots", "version": "1.0.0"},
        },
    )
    assert "result" in init, f"initialize failed: {init}"
    client.notify("notifications/initialized")
    return proc, client


def _tool_text(result: dict) -> str:
    return "".join(
        block.get("text", "")
        for block in result.get("content", [])
        if block.get("type") == "text"
    )


def _workspace_from_info(client: RootsAwareClient) -> str:
    result = client.call_tool("workspace_info")
    payload = json.loads(_tool_text(result))
    return payload["session"]["workspace"]


def test_client_root_reroots_workspace(daimonos_binary, tmp_path):
    """A client-advertised root overrides the hardcoded `-w` launch path."""
    launch_ws = tmp_path / "launch_dir"
    project = tmp_path / "real_project"
    launch_ws.mkdir()
    project.mkdir()
    (project / "marker.txt").write_text("real project file\n")

    root_uri = f"file://{os.path.realpath(project)}"
    proc, client = _spawn(daimonos_binary, launch_ws, [{"uri": root_uri, "name": "proj"}])
    try:
        # on_initialized runs asynchronously right after the initialized
        # notification; give the re-root a moment to land.
        deadline = time.time() + 5
        ws = _workspace_from_info(client)
        while ws != os.path.realpath(project) and time.time() < deadline:
            time.sleep(0.1)
            ws = _workspace_from_info(client)
        assert ws == os.path.realpath(project), (
            f"expected re-root to {project}, got {ws}"
        )
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_invalid_root_falls_back_to_launch_workspace(daimonos_binary, tmp_path):
    """A non-existent root is ignored; the launch workspace is retained."""
    launch_ws = tmp_path / "launch_dir"
    launch_ws.mkdir()

    bad_uri = "file:///definitely/not/a/real/dir/zzzqqq"
    proc, client = _spawn(daimonos_binary, launch_ws, [{"uri": bad_uri, "name": "ghost"}])
    try:
        # Allow on_initialized to run (and decline to re-root).
        time.sleep(1.0)
        ws = _workspace_from_info(client)
        assert ws == os.path.realpath(launch_ws), (
            f"expected fallback to {launch_ws}, got {ws}"
        )
    finally:
        proc.terminate()
        proc.wait(timeout=5)
