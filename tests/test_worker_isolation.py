"""WP10 credential-free worker execution — observable CLI behavior (FR-05, ADR-028).

Context: docs/adr/028-worker-only-execution.md,
docs/contexts/reviewer-runtime/INTERFACES.md (WP10 rows), tests/README.md.

Integration tests: they drive the real `daimonos agent --worker-config` process
against a local scripted model endpoint (no live API, no real credentials) and a
RECORDING credential-free MCP-stdio worker. The worker records its own process
environment (so AC-05.1 can assert no credential reaches it) and every tool call
it serves (so AC-05.3 can assert routing). A worker that no network reaches is
approximated here by a pure-stdlib worker that makes no outbound connection; the
no-network container leg (AC-05.2) and the capability probe leg (WP09) are merge
items — see the handoff.

ACs covered: AC-05.1 (credential-free env), AC-05.3 (every call served by a
worker, none in-process), AC-05.4 (killed worker -> distinct error, no later
tool), AC-05.5 (caller tool allowlist). Plus the default-off regression and the
fail-closed worker-config validation.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest


CREDENTIAL = "synthetic-super-secret-credential-value"

# A recording, credential-free MCP-stdio worker (pure stdlib, newline-delimited
# JSON-RPC, the same framing daimonos's own MCP stdio server uses). It dumps its
# environment once at startup and appends every served call, so the test can
# inspect isolation and routing from the outside. The `crash` tool makes the
# worker exit mid-call to simulate the tool side dying (AC-05.4).
WORKER_SRC = r'''
import json, os, sys

record = os.environ.get("WORKER_RECORD")
if record:
    with open(record, "w") as f:
        json.dump({"env": dict(os.environ)}, f)
# AC-05.2 no-network proof: when the caller wraps this worker in a network
# namespace (e.g. `unshare -Urn`), a connect to the model endpoint address must
# fail. The outcome is written to WORKER_NETCHECK so the test can assert it from
# outside. Only exercised when both vars are set.
_target = os.environ.get("NETCHECK_TARGET")
_tfile = os.environ.get("NETCHECK_TARGET_FILE")
if _tfile and not _target:
    try:
        with open(_tfile) as _tf:
            _target = _tf.read().strip()
    except OSError:
        _target = None
_netout = os.environ.get("WORKER_NETCHECK")
if _target and _netout:
    import socket
    _host, _, _port = _target.rpartition(":")
    try:
        _s = socket.create_connection((_host, int(_port)), 2)
        _s.close()
        _result = "connected"
    except OSError as _e:
        _result = "failed: %s" % _e
    with open(_netout, "w") as _f:
        _f.write(_result)
calls_path = os.environ.get("WORKER_CALLS")

def log_call(name):
    if calls_path:
        with open(calls_path, "a") as f:
            f.write(name + "\n")

TOOLS = [
    {"name": "read_note", "description": "Return a fixed note.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "crash", "description": "Exit the worker process.",
     "inputSchema": {"type": "object", "properties": {}}},
]

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        mid = msg.get("id")
        method = msg.get("method")
        if method == "initialize":
            pv = msg.get("params", {}).get("protocolVersion", "2025-06-18")
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": pv,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "recording-worker", "version": "1.0.0"},
            }})
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
        elif method == "tools/call":
            name = msg.get("params", {}).get("name")
            log_call(name)
            if name == "crash":
                sys.stdout.flush()
                os._exit(1)
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": os.environ.get(
                    "WORKER_NOTE_TEXT", "note: hello from the worker")}],
                "isError": False,
            }})
        elif mid is not None:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32601, "message": "method not found"}})

main()
'''


# A worker that both (a) keeps running after its stdin closes (a NON-daemon
# thread outlives `main()`) and (b) spawns a long-lived GRANDCHILD in its own
# process group. The SDK's `kill_on_drop` reaps only the DIRECT child at process
# exit; the grandchild orphans to init and survives unless teardown reaps the
# whole process GROUP (R6 — a stalled `shut_down` must still terminate the
# child and its descendants). No signal handler is installed, so SIGTERM/SIGKILL
# ends them.
WORKER_STUBBORN_SRC = r'''
import json, os, subprocess, sys, threading, time

_marker = os.environ.get("GRANDCHILD_MARKER", "wp10-stubborn-grandchild")
subprocess.Popen([sys.executable, "-c",
                  "import time\nwhile True:\n    time.sleep(3600)", _marker])

def _idle():
    while True:
        time.sleep(3600)

threading.Thread(target=_idle, daemon=False).start()

TOOLS = [{"name": "read_note", "description": "note",
          "inputSchema": {"type": "object", "properties": {}}}]

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()

def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line); mid = msg.get("id"); method = msg.get("method")
        if method == "initialize":
            pv = msg.get("params", {}).get("protocolVersion", "2025-06-18")
            send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": pv,
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "stubborn-worker", "version": "1.0.0"}}})
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
        elif method == "tools/call":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                  "content": [{"type": "text", "text": "ok"}], "isError": False}})
        elif mid is not None:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32601, "message": "no"}})

main()
'''


def _write_worker(tmp_path):
    script = tmp_path / "worker.py"
    script.write_text(WORKER_SRC)
    record = tmp_path / "worker_env.json"
    calls = tmp_path / "worker_calls.txt"
    config = tmp_path / "worker_config.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": sys.executable,
        "args": [str(script)],
        # The COMPLETE, explicit, credential-free child environment. PATH lets
        # env -i's child find shared libs if needed; the record/calls paths are
        # the test's observation channel. No credential is listed.
        "env": {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "WORKER_RECORD": str(record),
            "WORKER_CALLS": str(calls),
            # Suppress CPython's PEP 538 C-locale coercion, which otherwise
            # has the interpreter set LC_CTYPE on ITSELF at startup under the
            # cleared `env -i` environment (not a daimonos leak). Pinning it
            # keeps the worker's recorded environment EXACTLY the caller's
            # explicit map, so the AC-05.1 exact-set assertion proves the
            # `env -i` wrapper adds nothing of daimonos's own.
            "PYTHONCOERCECLOCALE": "0",
        },
    }}}))
    return config, record, calls


class _MockChat:
    """Local scripted OpenAI/OpenRouter Chat Completions endpoint (SSE)."""

    def __init__(self, turns):
        self._turns = list(turns)
        self._index = 0
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{
                    "id": "test/model", "created": 1, "context_length": 100000,
                    "supported_parameters": ["tools"],
                    "pricing": {"prompt": "0", "completion": "0"},
                }]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                index = min(owner._index, len(owner._turns) - 1)
                owner._index += 1
                t = owner._turns[index]
                delta = {"role": "assistant"}
                if t.get("text") is not None:
                    delta["content"] = t["text"]
                if t.get("calls"):
                    delta["tool_calls"] = [{
                        "index": i, "id": c["id"], "type": "function",
                        "function": {"name": c["name"],
                                     "arguments": json.dumps(c.get("args", {}))},
                    } for i, c in enumerate(t["calls"])]
                finish = "tool_calls" if t.get("calls") else "stop"
                chunk = {"choices": [{"index": 0, "delta": delta,
                                      "finish_reason": finish}],
                         "usage": {"prompt_tokens": 10, "completion_tokens": 5}}
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                try:
                    self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self):
        return self.server.server_port

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


def _run(binary, tmp_path, turns, *, worker_config=None, allowlist=None,
         audit_path=None, read_only=False, task="do the work",
         final_output=None, extra_config="", port_file=None):
    mock = _MockChat(turns)
    try:
        if port_file is not None:
            # Publish the model endpoint to the worker (which runs under `env -i`
            # and cannot see the host env) BEFORE daimonos launches it.
            port_file.write_text("127.0.0.1:%d" % mock.port)
        home = tmp_path / "home"
        home.mkdir(exist_ok=True)
        # Workspace (evidence) is a subdir so state/audit destinations live
        # OUTSIDE it, as the read-only/audit rules require.
        ws = tmp_path / "ws"
        ws.mkdir(exist_ok=True)
        state = tmp_path / "state"
        state.mkdir(exist_ok=True)
        os.chmod(state, 0o700)
        config = tmp_path / "config.toml"
        config.write_text(
            "[analytics]\nenabled=false\n[acp.mcp]\nenabled=false\n" + extra_config)
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter",
            "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{mock.port}",
            f"DAIMONOS_AGENT_API_KEY={CREDENTIAL}",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_COMPACTION=off", ""]))
        argv = [binary, "--workspace", str(ws), "--state-dir", str(state),
                "--config", str(config)]
        if read_only:
            argv.append("--workspace-read-only")
        if allowlist is not None:
            allow = tmp_path / "allow.json"
            allow.write_text(json.dumps(allowlist))
            argv += ["--tool-allowlist", str(allow)]
        if audit_path is not None:
            argv += ["--audit-path", str(audit_path)]
        argv += ["agent"]
        if worker_config is not None:
            argv += ["--worker-config", str(worker_config)]
        if final_output is not None:
            argv += ["--final-output", str(final_output)]
        argv += ["--agent-env", str(agent_env), "--print", task]
        # The credential is a real env var of the MODEL-SIDE process, so clearing
        # it for the worker is a meaningful guarantee (AC-05.1), plus a second
        # independent carrier that is not a known provider var.
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("DAIMONOS_", "XDG_"))}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                   DAIMONOS_AGENT_API_KEY=CREDENTIAL, SECRET_TOKEN=CREDENTIAL)
        completed = subprocess.run(argv, capture_output=True, text=True,
                                   timeout=120, env=env)
        return completed
    finally:
        mock.close()


def _require_flag(binary):
    help_text = subprocess.run([binary, "agent", "--help"], capture_output=True,
                               text=True, timeout=30)
    assert "--worker-config" in (help_text.stdout + help_text.stderr), (
        "`agent --worker-config` must be wired; a missing flag fails this suite.")


@pytest.fixture(scope="module", autouse=True)
def _require(daimonos_binary):
    _require_flag(daimonos_binary)


NOTE_CALL = {"id": "c1", "name": "mcp__toolside__read_note"}
CRASH_CALL = {"id": "c1", "name": "mcp__toolside__crash"}
DONE = {"text": "done"}


def test_ac_05_1_worker_env_has_no_credential(daimonos_binary, tmp_path):
    config, record, _ = _write_worker(tmp_path)
    completed = _run(daimonos_binary, tmp_path,
                     [{"calls": [NOTE_CALL]}, DONE], worker_config=config)
    assert completed.returncode == 0, completed.stderr
    worker_env = json.loads(record.read_text())["env"]
    # No variable's VALUE equals the credential (AC-05.1).
    assert CREDENTIAL not in set(worker_env.values()), worker_env
    # And the obvious credential carriers are entirely absent from the child.
    assert "DAIMONOS_AGENT_API_KEY" not in worker_env
    assert "SECRET_TOKEN" not in worker_env
    # Only the caller's explicit map is present (PYTHONCOERCECLOCALE is part of
    # that explicit map; see _write_worker for why it is pinned).
    assert set(worker_env.keys()) <= {
        "PATH", "WORKER_RECORD", "WORKER_CALLS", "PYTHONCOERCECLOCALE"}


def test_ac_05_3_every_call_routed_through_worker(daimonos_binary, tmp_path):
    config, _, calls = _write_worker(tmp_path)
    # Audit sink must live inside the state dir (and outside the workspace).
    audit = tmp_path / "state" / "audit.jsonl"
    completed = _run(daimonos_binary, tmp_path,
                     [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, audit_path=audit)
    assert completed.returncode == 0, completed.stderr
    # The worker actually served the call.
    assert "read_note" in calls.read_text().split()
    # The audit marks the dispatch as served by a worker, never in-process.
    events = [json.loads(line) for line in audit.read_text().splitlines() if line.strip()]
    tool_calls = [e for e in events if e.get("kind") == "tool_call"]
    assert tool_calls, events
    assert all(e.get("route") == "worker" for e in tool_calls), tool_calls


def test_ac_05_4_killed_worker_aborts_distinctly_with_no_later_tool(daimonos_binary, tmp_path):
    config, _, calls = _write_worker(tmp_path)
    # turn 1 crashes the worker; a scripted turn 2 would call read_note, which
    # must NEVER run because the run aborts on the lost worker.
    completed = _run(daimonos_binary, tmp_path,
                     [{"calls": [CRASH_CALL]}, {"calls": [NOTE_CALL]}, DONE],
                     worker_config=config)
    # Reviewed exit table (INTERFACES.md): a lost worker route exits 22.
    assert completed.returncode == 22, completed.stdout + completed.stderr
    assert "worker_unavailable" in (completed.stdout + completed.stderr)
    served = calls.read_text().split() if calls.exists() else []
    assert "crash" in served
    assert "read_note" not in served, "a tool ran after the worker died (no fallback allowed)"


def test_wp08_worker_death_is_recorded_in_the_audit(daimonos_binary, tmp_path):
    # WP08 (ADR-026) item: a worker dying during an AUDITED run must be recorded,
    # and the run must end per ADR-026 (terminal run_end, non-zero exit). The
    # dead dispatch is recorded at the boundary with the distinct
    # `worker_unavailable` reason under the `worker` route; the terminal
    # `run_end` carries status=error. No tool runs after the death.
    config, _, calls = _write_worker(tmp_path)
    audit = tmp_path / "state" / "audit.jsonl"
    completed = _run(daimonos_binary, tmp_path,
                     [{"calls": [CRASH_CALL]}, {"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, audit_path=audit)
    assert completed.returncode != 0, completed.stdout
    events = [json.loads(line) for line in audit.read_text().splitlines() if line.strip()]
    # The worker death is recorded at the dispatch boundary, distinctly.
    death = [e for e in events
             if e.get("reason") == "worker_unavailable" and e.get("route") == "worker"]
    assert death, events
    # The run ends per ADR-026: a terminal run_end whose status and exit_code are
    # the reviewed worker_unavailable failure (INTERFACES.md exit table: 22).
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert run_end, events
    assert run_end[-1].get("status") == "worker_unavailable", run_end
    assert run_end[-1].get("exit_code") == 22, run_end
    # The process exit agrees with the recorded exit_code.
    assert completed.returncode == 22, completed.stderr
    # No tool ran after the worker died.
    served = calls.read_text().split() if calls.exists() else []
    assert "crash" in served and "read_note" not in served, served


def test_ac_05_5_caller_tool_allowed_when_listed(daimonos_binary, tmp_path):
    config, _, calls = _write_worker(tmp_path)
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config,
                     allowlist=["mcp__toolside__read_note"])
    assert completed.returncode == 0, completed.stderr
    assert "read_note" in calls.read_text().split()


def test_ac_05_5_caller_tool_refused_when_not_listed(daimonos_binary, tmp_path):
    config, _, calls = _write_worker(tmp_path)
    # read_note is NOT on the allowlist -> refused BEFORE reaching the worker.
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config,
                     allowlist=["mcp__toolside__crash"])
    assert completed.returncode == 0, completed.stderr
    served = calls.read_text().split() if calls.exists() else []
    assert "read_note" not in served, (
        "a non-allowlisted worker tool was executed")


def test_wp04_worker_tool_not_refused_under_read_only(daimonos_binary, tmp_path):
    # PM FR-05: worker tools are the caller's sandbox; --workspace-read-only does
    # not refuse or path-check them. Admission rule (file+symbol): the only gate
    # is run_policy::ToolPolicy::allows at the agent.rs dispatch boundary.
    config, _, calls = _write_worker(tmp_path)
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, read_only=True)
    assert completed.returncode == 0, completed.stderr
    assert "read_note" in calls.read_text().split()


def test_default_off_regression_no_worker(daimonos_binary, tmp_path):
    # Without --worker-config the agent runs exactly as today: native tools, no
    # worker spawned. A plain text turn ends the run.
    _write_worker(tmp_path)  # fixtures exist but are not referenced
    completed = _run(daimonos_binary, tmp_path, [{"text": "done"}])
    assert completed.returncode == 0, completed.stderr
    assert "done" in completed.stdout


def test_worker_config_invalid_is_fail_closed(daimonos_binary, tmp_path):
    bad = tmp_path / "bad.json"
    bad.write_text("{ not json")
    completed = _run(daimonos_binary, tmp_path, [DONE], worker_config=bad)
    # Reviewed exit table (INTERFACES.md): a bad control file exits 2.
    assert completed.returncode == 2, completed.stderr
    assert "run_configuration_invalid" in completed.stderr


def test_worker_config_empty_is_fail_closed(daimonos_binary, tmp_path):
    empty = tmp_path / "empty.json"
    empty.write_text(json.dumps({"workers": {}}))
    completed = _run(daimonos_binary, tmp_path, [DONE], worker_config=empty)
    assert completed.returncode == 2, completed.stderr
    assert "run_configuration_invalid" in completed.stderr


def test_missing_worker_aborts_without_local_fallback(daimonos_binary, tmp_path):
    # A worker command that cannot launch makes the opt-in route unusable by
    # design (ADR-028): the run fails closed, never falling back to local tools.
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"gone": {
        "command": str(tmp_path / "does-not-exist"), "env": {}}}}))
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config)
    # A required worker that cannot launch exits 22 (no local fallback).
    assert completed.returncode == 22, completed.stderr
    assert "worker_unavailable" in (completed.stdout + completed.stderr)


# --- r2 review fixes (R1-R8) -------------------------------------------------

FINAL_SCHEMA = {
    "type": "object",
    "properties": {"answer": {"type": "string"}},
    "required": ["answer"],
    "additionalProperties": False,
}


def _write_final_control(tmp_path, tool_name="final_answer"):
    control = tmp_path / "final.json"
    control.write_text(json.dumps({
        "schema": FINAL_SCHEMA, "tool_name": tool_name,
        "tool_description": "Emit the final structured result.",
        "corrective_retries": 0}))
    return control


def _worker_survivors(needle):
    """PIDs whose /proc cmdline contains `needle` — used to prove, via the
    process table, that no owned worker child outlives the run (R6)."""
    survivors = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open("/proc/%s/cmdline" % pid, "rb") as f:
                cmd = f.read().replace(b"\x00", b" ").decode("utf-8", "replace")
        except OSError:
            continue
        if needle in cmd:
            survivors.append(pid)
    return survivors


def test_r1_worker_env_disclosing_credential_is_refused(daimonos_binary, tmp_path):
    # ADR-028: an explicit worker `env` value that discloses the model credential
    # is refused BEFORE the worker starts (exit 2), and the diagnostic never
    # contains the credential value.
    script = tmp_path / "worker.py"
    script.write_text(WORKER_SRC)
    record = tmp_path / "worker_env.json"
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": sys.executable,
        "args": [str(script)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "WORKER_RECORD": str(record),
                "PROVIDER_KEY": CREDENTIAL},
    }}}))
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config)
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "run_configuration_invalid" in completed.stderr
    assert "worker_credential_disclosure" in completed.stderr
    # No worker launched: its startup record was never written.
    assert not record.exists(), "a worker launched despite credential disclosure"
    # The credential value never appears in the diagnostic.
    assert CREDENTIAL not in (completed.stdout + completed.stderr)


def test_r1_worker_args_disclosing_credential_is_refused(daimonos_binary, tmp_path):
    script = tmp_path / "worker.py"
    script.write_text(WORKER_SRC)
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": sys.executable,
        "args": [str(script), "--api-key=" + CREDENTIAL],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin")},
    }}}))
    completed = _run(daimonos_binary, tmp_path, [DONE], worker_config=config)
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "worker_credential_disclosure" in completed.stderr
    assert CREDENTIAL not in (completed.stdout + completed.stderr)


def test_ac_05_2_worker_proves_no_network_in_namespace(daimonos_binary, tmp_path):
    # AC-05.2 no-network leg: the worker is launched inside a user+net namespace
    # with no network (`unshare -Urn`); the fake model endpoint is reachable only
    # from the model side. The complete run succeeds AND the worker proves it has
    # no network (a connect to the endpoint from inside the namespace fails).
    unshare = shutil.which("unshare")
    if unshare is None:
        pytest.skip("unshare not available on host")
    probe = subprocess.run([unshare, "-Urn", "true"], capture_output=True)
    if probe.returncode != 0:
        pytest.skip("host cannot create an unprivileged user+net namespace: "
                    + probe.stderr.decode("utf-8", "replace").strip())
    script = tmp_path / "worker.py"
    script.write_text(WORKER_SRC)
    calls = tmp_path / "worker_calls.txt"
    netcheck = tmp_path / "worker_netcheck.txt"
    target_file = tmp_path / "endpoint.txt"
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": unshare,
        "args": ["-Urn", sys.executable, str(script)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "WORKER_CALLS": str(calls),
                "WORKER_NETCHECK": str(netcheck),
                "NETCHECK_TARGET_FILE": str(target_file),
                "PYTHONCOERCECLOCALE": "0"},
    }}}))
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, port_file=target_file)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert "read_note" in calls.read_text().split()
    assert netcheck.exists(), "worker did not record its network check"
    result = netcheck.read_text()
    assert result.startswith("failed"), (
        "worker reached the model endpoint from inside its namespace: " + result)


def test_r3_unknown_worker_allowlist_entry_fails_at_startup(daimonos_binary, tmp_path):
    # FR-02 req5/req7: after discovery, a worker-scoped allowlist entry that no
    # worker serves fails the run at startup (exit 2), naming the entry.
    config, _, _ = _write_worker(tmp_path)
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config,
                     allowlist=["mcp__toolside__read_note", "mcp__ghost__missing"])
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "run_configuration_invalid" in completed.stderr
    assert "tool_allowlist_unknown_entry" in completed.stderr
    assert "mcp__ghost__missing" in completed.stderr


def test_r4_final_only_run_dispatches_nothing_to_worker(daimonos_binary, tmp_path):
    # PM FR-05 / checkpoint: with --final-output AND --worker-config, a run in
    # which the model calls only the final-answer tool ends model-side, is
    # validated and emitted on stdout, and dispatches NOTHING to a worker nor
    # records anything under the `worker` route.
    config, _, calls = _write_worker(tmp_path)
    control = _write_final_control(tmp_path)
    audit = tmp_path / "state" / "audit.jsonl"
    final = {"id": "f1", "name": "final_answer", "args": {"answer": "42"}}
    completed = _run(daimonos_binary, tmp_path, [{"calls": [final]}],
                     worker_config=config, final_output=control, audit_path=audit)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    emitted = json.loads(completed.stdout.strip().splitlines()[-1])
    assert emitted == {"answer": "42"}, completed.stdout
    # Nothing dispatched to a worker.
    served = calls.read_text().split() if calls.exists() else []
    assert served == [], served
    events = [json.loads(line) for line in audit.read_text().splitlines() if line.strip()]
    worker_calls = [e for e in events
                    if e.get("kind") == "tool_call" and e.get("route") == "worker"]
    assert worker_calls == [], worker_calls


def test_r6_no_worker_child_survives_normal_abort_or_final_failure(daimonos_binary, tmp_path):
    config, _, _ = _write_worker(tmp_path)
    script = str(tmp_path / "worker.py")
    # Normal run.
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config)
    assert completed.returncode == 0, completed.stderr
    assert _worker_survivors(script) == [], "worker survived a normal run"
    # Worker-abort run (exit 22).
    completed = _run(daimonos_binary, tmp_path, [{"calls": [CRASH_CALL]}, DONE],
                     worker_config=config)
    assert completed.returncode == 22, completed.stderr
    assert _worker_survivors(script) == [], "worker survived an aborted run"
    # Final-output failure (final answer missing a required field).
    control = _write_final_control(tmp_path)
    bad_final = {"id": "f1", "name": "final_answer", "args": {}}
    completed = _run(daimonos_binary, tmp_path, [{"calls": [bad_final]}],
                     worker_config=config, final_output=control)
    assert completed.returncode != 0, completed.stdout
    assert _worker_survivors(script) == [], "worker survived a final-output failure"


def test_r6_stalled_worker_is_force_reaped(daimonos_binary, tmp_path):
    # A worker that ignores stdin closure (a non-daemon thread outlives main)
    # must still be terminated by teardown: a `shut_down` that cannot reap it
    # gracefully force-kills its process group.
    script = tmp_path / "stubborn.py"
    script.write_text(WORKER_STUBBORN_SRC)
    marker = str(script) + "-grandchild"
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"stub": {
        "command": sys.executable, "args": [str(script)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "GRANDCHILD_MARKER": marker,
                "PYTHONCOERCECLOCALE": "0"}}}}))
    stub_call = {"id": "c1", "name": "mcp__stub__read_note"}
    completed = _run(daimonos_binary, tmp_path, [{"calls": [stub_call]}, DONE],
                     worker_config=config, extra_config="shutdown_timeout_secs = 2\n")
    assert completed.returncode == 0, completed.stdout + completed.stderr
    # Neither the stalled worker nor its grandchild (in the worker's process
    # group) may outlive the run; teardown force-reaps the whole group.
    assert _worker_survivors(str(script)) == [], (
        "a stalled worker survived teardown; shut_down did not reap it")
    assert _worker_survivors(marker) == [], (
        "a worker descendant survived teardown; the process group was not reaped")


def test_r7_worker_result_text_with_token_does_not_exit_22(daimonos_binary, tmp_path):
    # A worker tool RESULT whose text merely contains the word `worker_unavailable`
    # must NOT be misread as a lost worker: the run ends normally (exit 0). The
    # worker-loss exit mapping keys on a TYPED terminal reason, not this substring.
    script = tmp_path / "worker.py"
    script.write_text(WORKER_SRC)
    calls = tmp_path / "calls.txt"
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": sys.executable, "args": [str(script)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "WORKER_CALLS": str(calls),
                "WORKER_NOTE_TEXT": "status: worker_unavailable observed in a tool result",
                "PYTHONCOERCECLOCALE": "0"}}}}))
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert "read_note" in calls.read_text().split()


def test_r8_command_with_equals_is_refused_at_parse(daimonos_binary, tmp_path):
    # ADR-028: a `command` containing `=` would be misparsed by the `env`
    # launcher; it is refused at parse (exit 2), not left to fail opaquely as
    # worker_unavailable (exit 22).
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": "FOO=bar", "env": {}}}}))
    completed = _run(daimonos_binary, tmp_path, [DONE], worker_config=config)
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "run_configuration_invalid" in completed.stderr
    assert "command" in completed.stderr


# --- r3 D33 fixes (K1/K2/K3): terminal record and process exit always agree ---


def test_k1_unknown_allowlist_entry_records_run_end_and_reaps_worker(
        daimonos_binary, tmp_path):
    # D33/K1: a worker-scoped allowlist entry that no worker serves can be
    # decided only AFTER a side effect (the worker was started and discovered).
    # It must end through the single terminal source: the audit carries
    # run_start AND exactly one run_end with status run_configuration_invalid and
    # exit_code 2 (never a lone run_start, which is indistinguishable from a
    # killed process), the process exits 2 naming the entry, and no worker child
    # survives.
    config, _, _ = _write_worker(tmp_path)
    script = str(tmp_path / "worker.py")
    audit = tmp_path / "state" / "audit.jsonl"
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, audit_path=audit,
                     allowlist=["mcp__toolside__read_note", "mcp__ghost__missing"])
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "run_configuration_invalid" in completed.stderr
    assert "tool_allowlist_unknown_entry" in completed.stderr
    assert "mcp__ghost__missing" in completed.stderr
    events = [json.loads(line) for line in audit.read_text().splitlines()
              if line.strip()]
    kinds = [e.get("kind") for e in events]
    assert kinds.count("run_start") == 1, events
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert len(run_end) == 1, events
    assert run_end[0].get("status") == "run_configuration_invalid", run_end
    assert run_end[0].get("exit_code") == 2, run_end
    # The worker was started (the side effect) but reaped before the exit.
    assert _worker_survivors(script) == [], "a worker survived the config refusal"


def test_k2_credential_disclosure_leaves_no_audit_file_and_no_worker(
        daimonos_binary, tmp_path):
    # D33/K2: a worker `env` value that discloses the model credential is a pure
    # configuration refusal needing no runtime state. It is refused BEFORE the
    # audit opens, so it leaves NO audit file (the same as every other exit-2
    # configuration refusal), starts no worker, and the diagnostic never
    # contains the credential value.
    script = tmp_path / "worker.py"
    script.write_text(WORKER_SRC)
    record = tmp_path / "worker_env.json"
    audit = tmp_path / "state" / "audit.jsonl"
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"toolside": {
        "command": sys.executable,
        "args": [str(script)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "WORKER_RECORD": str(record),
                "PROVIDER_KEY": CREDENTIAL},
    }}}))
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, audit_path=audit)
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "worker_credential_disclosure" in completed.stderr
    assert CREDENTIAL not in (completed.stdout + completed.stderr)
    # No worker launched: its startup record was never written.
    assert not record.exists(), "a worker launched despite credential disclosure"
    # No audit file: the refusal precedes open_run_audit, so run_start was never
    # written (a lone run_start would be indistinguishable from a kill).
    assert not audit.exists(), (
        "an audit file was created for a pre-audit configuration refusal")


def test_k3_worker_init_failure_records_matching_run_end(daimonos_binary, tmp_path):
    # D33/K3 inventory gap: a required worker that cannot initialize is returned
    # as an Err from run_agent (not the in-run worker_lost flag). Its terminal
    # run_end must carry the SAME exit as the process (22 / worker_unavailable),
    # via the single terminal source, so an audited init failure is never
    # recorded as a generic error (exit 1) while the process exits 22.
    audit = tmp_path / "state" / "audit.jsonl"
    config = tmp_path / "wc.json"
    config.write_text(json.dumps({"workers": {"gone": {
        "command": str(tmp_path / "does-not-exist"), "env": {}}}}))
    completed = _run(daimonos_binary, tmp_path, [{"calls": [NOTE_CALL]}, DONE],
                     worker_config=config, audit_path=audit)
    assert completed.returncode == 22, completed.stdout + completed.stderr
    assert "worker_unavailable" in (completed.stdout + completed.stderr)
    events = [json.loads(line) for line in audit.read_text().splitlines()
              if line.strip()]
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert len(run_end) == 1, events
    assert run_end[0].get("status") == "worker_unavailable", run_end
    assert run_end[0].get("exit_code") == 22, run_end
