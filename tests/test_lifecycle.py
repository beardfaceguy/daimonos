"""Process lifecycle tests for daimonos --mcp.

Bug being prevented: a daimonos --mcp subprocess can be left running
indefinitely when its parent (an editor or external agent) abandons the
connection without closing stdin -- e.g. an editor worker leaks the pipe
write-end after closing the agent panel. Without an idle watchdog,
daimonos blocks forever in its read loop and accumulates resources
(inotify watches, memory, fds). After the fix the process must self-exit
once it has been idle for longer than the configured timeout.

Second bug (vikunja #1078): the watchdog measured idleness from the arrival
of the last request, so a *single* tool call that ran longer than the timeout
looked identical to an abandoned server. The process was killed mid-request,
the client lost the result, and the only symptom was an opaque transport
error. Any slow build, large install, or hung command could trigger it. The
watchdog must now treat an in-flight request as proof of liveness.
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


def test_idle_timeout_does_not_fire_during_one_long_running_call(
    daimonos_binary, tmp_path
):
    """A single call that outlasts the idle window must survive (vikunja #1078).

    This is the case repeated polling cannot catch: no new requests arrive
    while the call runs, so a timestamp-only watchdog counts it as idle.
    """
    idle_secs = 2
    sleep_secs = 6  # comfortably past the window, with slack for slow CI
    proc = _spawn(daimonos_binary, str(tmp_path), idle_secs=idle_secs)
    try:
        _handshake(proc)
        started = time.monotonic()
        resp = _send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 200,
                "method": "tools/call",
                "params": {
                    "name": "exec",
                    "arguments": {"command": f"sleep {sleep_secs}"},
                },
            },
        )
        elapsed = time.monotonic() - started

        # The call itself must come back. Before the fix the server exited
        # partway through and this read raised on a closed stdout.
        assert "result" in resp, f"long-running call did not return a result: {resp}"
        assert elapsed >= sleep_secs - 1, (
            f"call returned after only {elapsed:.1f}s; expected to block for "
            f"~{sleep_secs}s, so this did not exercise the long-call path"
        )
        assert proc.poll() is None, (
            "daimonos exited while a single tool call was still in flight -- an "
            "in-flight request must veto the idle watchdog"
        )

        # Still healthy afterwards, and the idle clock restarted from the
        # call's completion rather than its arrival.
        follow_up = _send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 201,
                "method": "tools/call",
                "params": {"name": "workspace_info", "arguments": {}},
            },
        )
        assert "result" in follow_up, f"server unusable after a long call: {follow_up}"
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


def test_idle_timeout_still_fires_after_a_long_call_completes(
    daimonos_binary, tmp_path
):
    """The veto must be released when the call ends, or the watchdog is dead.

    Guards against fixing #1078 by simply disabling the watchdog: once the
    in-flight count returns to zero the original leak protection has to work.
    """
    proc = _spawn(daimonos_binary, str(tmp_path), idle_secs=2)
    try:
        _handshake(proc)
        resp = _send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 300,
                "method": "tools/call",
                "params": {"name": "exec", "arguments": {"command": "sleep 4"}},
            },
        )
        assert "result" in resp, f"long call failed: {resp}"

        # Now go quiet. The slot is released, so the watchdog must reap us.
        try:
            exit_code = proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            pytest.fail(
                "daimonos did not self-exit after a long call finished and the "
                "session went idle -- the in-flight veto is never released"
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
