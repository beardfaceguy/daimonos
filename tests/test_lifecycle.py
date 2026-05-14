"""Process lifecycle tests for daimonos --mcp.

Bug being prevented: a daimonos --mcp subprocess can be left running
indefinitely when its parent (an editor or external agent) abandons the
connection without closing stdin -- e.g. an editor worker leaks the pipe
write-end after closing the agent panel. Without an idle watchdog,
daimonos blocks forever in its read loop and accumulates resources
(inotify watches, memory, fds). After the fix the process must self-exit
once it has been idle for longer than the configured timeout.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
from typing import Optional

import pytest


def _spawn(daimonos_binary: str, workspace: str, idle_secs: int) -> subprocess.Popen:
    env = os.environ.copy()
    env["DAIMONOS_IDLE_TIMEOUT_SECS"] = str(idle_secs)
    return subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", workspace],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )


def _send(proc: subprocess.Popen, msg: dict) -> Optional[dict]:
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()
    if "id" not in msg:
        return None
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("daimonos closed stdout unexpectedly")
    return json.loads(line)


def _handshake(proc: subprocess.Popen) -> None:
    init = _send(
        proc,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "lifecycle-test", "version": "1.0.0"},
            },
        },
    )
    assert "result" in init, f"initialize failed: {init}"
    _send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})


def test_idle_timeout_exits_when_client_abandons_pipe(daimonos_binary, tmp_path):
    """The leak scenario: parent stays alive (stdin write-end stays open)
    but never sends another message. daimonos must exit on its own."""
    proc = _spawn(daimonos_binary, str(tmp_path), idle_secs=2)
    try:
        _handshake(proc)
        # Do NOT close stdin. Just wait. The watchdog should fire within ~3s
        # (2s idle + ~1s slack). Generous overall timeout to avoid CI flake.
        try:
            exit_code = proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            pytest.fail(
                "daimonos --mcp did not self-exit within 10s of being idle "
                "(idle_timeout=2s). The process leaks when the parent holds "
                "stdin open but stops sending messages."
            )
        assert exit_code == 0, f"daimonos exited with non-zero status {exit_code}"
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def test_idle_timeout_does_not_fire_during_active_use(daimonos_binary, tmp_path):
    """Tool calls reset the idle clock so an active session is never killed."""
    proc = _spawn(daimonos_binary, str(tmp_path), idle_secs=3)
    try:
        _handshake(proc)
        # Keep poking the server faster than the idle window; over a span
        # longer than idle_secs it must still be alive.
        deadline = time.monotonic() + 5.0
        call_id = 100
        while time.monotonic() < deadline:
            call_id += 1
            resp = _send(
                proc,
                {
                    "jsonrpc": "2.0",
                    "id": call_id,
                    "method": "tools/call",
                    "params": {"name": "workspace_info", "arguments": {}},
                },
            )
            assert "result" in resp, f"tool call failed mid-session: {resp}"
            time.sleep(0.5)
        assert proc.poll() is None, (
            "daimonos exited during active use -- idle watchdog must reset "
            "on every tool call"
        )
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
