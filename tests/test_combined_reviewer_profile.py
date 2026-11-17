"""Combined reviewer profile: capability probe (WP09) + caller-owned workers (WP10).

Neither package could test the two features together. These drive the REAL
`daimonos agent --print` process with BOTH `--capability-probe-payload` (WP09,
ADR-027) and `--worker-config` (WP10, ADR-028) active, plus `--model-profile`,
`--final-output`, `--tool-allowlist` (one worker tool plus the final-answer
tool), and `--audit-path`. Only local fake endpoints are used; no live API and
no real credential value ever leaves the model-side process.

Helpers are reused from the two package suites:
- tests/test_capability_probe.py: the controlled-profile / final-output /
  payload writers, the `_env` builder, `_read_events`, and the FINAL_* /
  CRED_* constants (the probe credential is a real var of the model side).
- tests/test_worker_isolation.py: the recording credential-free MCP-stdio
  worker source and its `_write_worker` config builder.

The model endpoint here is the controlled transport named by `--model-profile`
(Messages wire), so ONE local endpoint serves both the single probe request and
the agent loop's requests; the worker is a separate credential-free child.
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

from test_capability_probe import (
    CRED_ENV,
    CRED_VALUE,
    FINAL_SCHEMA,
    FINAL_TOOL,
    _env,
    _read_events,
    _write_final_output,
    _write_payload,
    _write_profile,
)
from test_worker_isolation import WORKER_SRC, _worker_survivors, _write_worker

TIMEOUT = 120

WORKER_READ = "mcp__toolside__read_note"
WORKER_CRASH = "mcp__toolside__crash"


# --------------------------------------------------------------------------
# A scripted Messages-wire endpoint: it serves a fixed sequence of assistant
# responses (one per POST) and records every request body, so a test can both
# drive a multi-turn run (probe -> worker call -> final) and assert the exact
# tool catalog the probe and the first loop request offered.
# --------------------------------------------------------------------------
def _final(verdict="supported"):
    return {"content": [{"type": "tool_use", "id": "f1", "name": FINAL_TOOL,
                         "input": {"verdict": verdict}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}}


def _worker_call(tool, cid="w1"):
    return {"content": [{"type": "tool_use", "id": cid, "name": tool,
                         "input": {}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}}


def _text(txt="I believe so"):
    return {"content": [{"type": "text", "text": txt}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}}


class ScriptEndpoint:
    """Local fake Messages endpoint serving a scripted response sequence."""

    def __init__(self, script):
        self._script = list(script)
        self._index = 0
        self.requests = []  # recorded POST bodies, in order
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    owner.requests.append(json.loads(raw) if raw else None)
                except ValueError:
                    owner.requests.append(None)
                if owner._script:
                    idx = min(owner._index, len(owner._script) - 1)
                    body = owner._script[idx]
                else:
                    body = _text()
                owner._index += 1
                payload = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            def do_GET(self):
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", "13")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(b'{"data": []}')

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def url(self):
        host, port = self.server.server_address
        return f"http://{host}:{port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self._thread.join(timeout=3)


def _run_combined(daimonos_binary, tmp_path, script, *, allowlist,
                  worker_config, audit_path=None, read_only=False,
                  payload=None, max_model_requests=None, endpoint_url=None,
                  port_file=None, task="review the evidence"):
    """Drive `agent --print` with probe + worker + final-output + allowlist.

    Returns (CompletedProcess, recorded_request_bodies). When `endpoint_url`
    is given no local endpoint is started here (the caller owns one, e.g. to
    reach it from the model side only); the returned request list is empty.
    """
    workspace = tmp_path / "workspace"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    final_f = _write_final_output(tmp_path / "final.json", max_model_requests)
    payload_f = _write_payload(tmp_path / "payload.json", payload)
    allow_f = tmp_path / "allow.json"
    allow_f.write_text(json.dumps(allowlist))
    agent_env, env = _env(tmp_path)

    endpoint = None
    if endpoint_url is None:
        endpoint = ScriptEndpoint(script)
        endpoint_url = endpoint.url
        if port_file is not None:
            # Publish the model endpoint to a worker that runs under `env -i`
            # (and inside a namespace) BEFORE daimonos launches it.
            host, port = endpoint.server.server_address
            port_file.write_text("%s:%d" % (host, port))
    profile_f = _write_profile(tmp_path / "profile.json", endpoint_url)

    argv = [daimonos_binary, "--workspace", str(workspace),
            "--state-dir", str(state)]
    if read_only:
        argv.append("--workspace-read-only")
    argv += ["--tool-allowlist", str(allow_f)]
    if audit_path is not None:
        argv += ["--audit-path", str(audit_path)]
    argv += ["agent", "--print", "--agent-env", str(agent_env),
             "--model-profile", str(profile_f),
             "--final-output", str(final_f),
             "--capability-probe-payload", str(payload_f),
             "--worker-config", str(worker_config),
             task]
    try:
        completed = subprocess.run(argv, capture_output=True, text=True,
                                   timeout=TIMEOUT, env=env)
        posts = list(endpoint.requests) if endpoint is not None else []
        return completed, posts
    finally:
        if endpoint is not None:
            endpoint.close()


def _tool_names(body):
    return [t["name"] for t in (body or {}).get("tools", [])]


# ==========================================================================
# C1 (both features, happy path): probe passes, the model calls the worker
# tool, then the final-answer tool. Exit 0; stdout is the final document only;
# the worker serves exactly one call; the probe request's tool catalog equals
# the first loop request's and both equal the allowlist; the audit records the
# full ok run with a `worker`-route tool_call; no credential anywhere.
# ==========================================================================
def test_c1_probe_pass_worker_call_then_final(daimonos_binary, tmp_path):
    worker_config, _record, calls = _write_worker(tmp_path)
    allowlist = [WORKER_READ, FINAL_TOOL]
    audit = tmp_path / "state" / "audit.jsonl"
    script = [_final(), _worker_call(WORKER_READ), _final()]
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, script,
        allowlist=allowlist, worker_config=worker_config, audit_path=audit)

    assert completed.returncode == 0, completed.stderr + completed.stdout
    # Stdout is the validated final document only.
    assert json.loads(completed.stdout) == {"verdict": "supported"}, completed.stdout
    # The worker served exactly one call (read_note), nothing in-process.
    assert calls.read_text().split() == ["read_note"], calls.read_text()
    # Probe (POST #1) and first loop request (POST #2) offered the same catalog,
    # and its names equal the allowlist (frozen once: worker tools + final).
    assert len(posts) >= 2, posts
    assert posts[0]["tools"] == posts[1]["tools"], (posts[0], posts[1])
    assert set(_tool_names(posts[0])) == set(allowlist), _tool_names(posts[0])
    # Audit: run_start, one model_supported probe, model_request(s), one worker
    # tool_call, run_end ok 0.
    events = _read_events(audit)
    kinds = [e.get("kind") for e in events]
    assert kinds.count("run_start") == 1, kinds
    probe = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe) == 1 and probe[0]["status"] == "model_supported", probe
    assert any(k == "model_request" for k in kinds), kinds
    tool_calls = [e for e in events if e.get("kind") == "tool_call"]
    assert len(tool_calls) == 1, tool_calls
    assert tool_calls[0].get("route") == "worker", tool_calls[0]
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert run_end and run_end[-1]["status"] == "ok", run_end
    assert run_end[-1]["exit_code"] == 0, run_end
    # The credential value appears in no output and not in the audit.
    assert CRED_VALUE not in (completed.stdout + completed.stderr)
    assert CRED_VALUE not in audit.read_text()


# ==========================================================================
# C2: the probe fails (text only). Exit 21, run_end model_unsupported 21, the
# worker served no call, no worker process survives, exactly one model request.
# ==========================================================================
def test_c2_probe_failure_stops_before_worker(daimonos_binary, tmp_path):
    worker_config, record, calls = _write_worker(tmp_path)
    allowlist = [WORKER_READ, FINAL_TOOL]
    audit = tmp_path / "state" / "audit.jsonl"
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, [_text()],
        allowlist=allowlist, worker_config=worker_config, audit_path=audit)

    assert completed.returncode == 21, completed.stderr + completed.stdout
    assert "model_unsupported" in (completed.stderr + completed.stdout)
    # Exactly one model request reached the wire: the probe.
    assert len(posts) == 1, posts
    # No worker tool was served before the failed probe.
    served = calls.read_text().split() if calls.exists() else []
    assert served == [], served
    # No owned worker child outlives the run.
    survivors = _worker_survivors(str(tmp_path / "worker.py"))
    assert survivors == [], survivors
    events = _read_events(audit)
    probe = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe) == 1 and probe[0]["status"] == "model_unsupported", probe
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert run_end and run_end[-1]["status"] == "model_unsupported", run_end
    assert run_end[-1]["exit_code"] == 21, run_end


# ==========================================================================
# C3: the probe passes and the final output fails (text only, no corrective
# retry). Exit 20; run_end structured_output_failed 20.
# ==========================================================================
def test_c3_probe_pass_then_final_output_fails(daimonos_binary, tmp_path):
    worker_config, _record, calls = _write_worker(tmp_path)
    allowlist = [WORKER_READ, FINAL_TOOL]
    audit = tmp_path / "state" / "audit.jsonl"
    # Probe passes (POST #1 final call); the loop then returns text only with no
    # final-answer call and corrective_retries=0 -> structured output fails.
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, [_final(), _text()],
        allowlist=allowlist, worker_config=worker_config, audit_path=audit)

    assert completed.returncode == 20, completed.stderr + completed.stdout
    events = _read_events(audit)
    probe = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe) == 1 and probe[0]["status"] == "model_supported", probe
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert run_end and run_end[-1]["status"] == "structured_output_failed", run_end
    assert run_end[-1]["exit_code"] == 20, run_end


# ==========================================================================
# C4: the probe passes and the worker dies during the call. Exit 22; run_end
# worker_unavailable 22.
# ==========================================================================
def test_c4_probe_pass_then_worker_dies(daimonos_binary, tmp_path):
    worker_config, _record, calls = _write_worker(tmp_path)
    # The one allowlisted worker tool is `crash`, which exits the worker mid-call.
    allowlist = [WORKER_CRASH, FINAL_TOOL]
    audit = tmp_path / "state" / "audit.jsonl"
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, [_final(), _worker_call(WORKER_CRASH)],
        allowlist=allowlist, worker_config=worker_config, audit_path=audit)

    assert completed.returncode == 22, completed.stderr + completed.stdout
    assert "worker_unavailable" in (completed.stdout + completed.stderr)
    served = calls.read_text().split() if calls.exists() else []
    assert "crash" in served, served
    events = _read_events(audit)
    probe = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe) == 1 and probe[0]["status"] == "model_supported", probe
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert run_end and run_end[-1]["status"] == "worker_unavailable", run_end
    assert run_end[-1]["exit_code"] == 22, run_end
    survivors = _worker_survivors(str(tmp_path / "worker.py"))
    assert survivors == [], survivors


# ==========================================================================
# C5 (AC-05.2 complete): the worker runs in a user+net namespace without
# network (`unshare -Urn`); the fake model endpoint is reachable from the model
# side only; the probe passes and the complete run ends with exit 0. Skipped
# only when the host cannot create the namespace.
# ==========================================================================
def test_c5_namespaced_worker_no_network_full_run_ok(daimonos_binary, tmp_path):
    unshare = shutil.which("unshare")
    if unshare is None:
        pytest.skip("unshare not available on host")
    probe = subprocess.run([unshare, "-Urn", "true"], capture_output=True)
    if probe.returncode != 0:
        pytest.skip("host cannot create an unprivileged user+net namespace: "
                    + probe.stderr.decode("utf-8", "replace").strip())
    script_py = tmp_path / "worker.py"
    script_py.write_text(WORKER_SRC)
    calls = tmp_path / "worker_calls.txt"
    netcheck = tmp_path / "worker_netcheck.txt"
    target_file = tmp_path / "endpoint.txt"
    worker_config = tmp_path / "wc.json"
    worker_config.write_text(json.dumps({"workers": {"toolside": {
        "command": unshare,
        "args": ["-Urn", sys.executable, str(script_py)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "WORKER_CALLS": str(calls),
                "WORKER_NETCHECK": str(netcheck),
                "NETCHECK_TARGET_FILE": str(target_file),
                "PYTHONCOERCECLOCALE": "0"},
    }}}))
    allowlist = [WORKER_READ, FINAL_TOOL]
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, [_final(), _worker_call(WORKER_READ), _final()],
        allowlist=allowlist, worker_config=worker_config, port_file=target_file)

    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert json.loads(completed.stdout) == {"verdict": "supported"}, completed.stdout
    assert "read_note" in calls.read_text().split()
    # The probe (model side) reached the endpoint; the worker (namespaced) did not.
    assert netcheck.exists(), "worker did not record its network check"
    assert netcheck.read_text().startswith("failed"), netcheck.read_text()


# ==========================================================================
# C6: C1 with `--workspace-read-only` added. Exit 0; the worker tool is not
# refused (worker tools are the caller's sandbox, not path-checked).
# ==========================================================================
def test_c6_read_only_does_not_refuse_worker_tool(daimonos_binary, tmp_path):
    worker_config, _record, calls = _write_worker(tmp_path)
    allowlist = [WORKER_READ, FINAL_TOOL]
    audit = tmp_path / "state" / "audit.jsonl"
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, [_final(), _worker_call(WORKER_READ), _final()],
        allowlist=allowlist, worker_config=worker_config, audit_path=audit,
        read_only=True)

    assert completed.returncode == 0, completed.stderr + completed.stdout
    assert json.loads(completed.stdout) == {"verdict": "supported"}, completed.stdout
    # The worker tool ran; it was NOT refused under read-only.
    assert calls.read_text().split() == ["read_note"], calls.read_text()
    events = _read_events(audit)
    tool_calls = [e for e in events if e.get("kind") == "tool_call"]
    assert len(tool_calls) == 1 and tool_calls[0].get("route") == "worker", tool_calls
    refusals = [e for e in events if e.get("kind") == "tool_refusal"]
    assert refusals == [], refusals
    run_end = [e for e in events if e.get("kind") == "run_end"]
    assert run_end and run_end[-1]["exit_code"] == 0, run_end


# ==========================================================================
# C7: a worker `env` value contains the credential, with the probe option set.
# Exit 2 (worker_credential_disclosure); no model request (so no probe request);
# no worker started; no audit file. The disclosure check precedes the audit and
# any worker/model request.
# ==========================================================================
def test_c7_worker_env_discloses_credential_refused_before_everything(
        daimonos_binary, tmp_path):
    script_py = tmp_path / "worker.py"
    script_py.write_text(WORKER_SRC)
    record = tmp_path / "worker_env.json"
    worker_config = tmp_path / "wc.json"
    # The worker env DISCLOSES the resolved model credential (CRED_VALUE), which
    # the disclosure check must catch before any worker launches.
    worker_config.write_text(json.dumps({"workers": {"toolside": {
        "command": sys.executable,
        "args": [str(script_py)],
        "env": {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "WORKER_RECORD": str(record),
                "LEAKED": CRED_VALUE},
    }}}))
    allowlist = [WORKER_READ, FINAL_TOOL]
    audit = tmp_path / "state" / "audit.jsonl"
    completed, posts = _run_combined(
        daimonos_binary, tmp_path, [_final(), _worker_call(WORKER_READ), _final()],
        allowlist=allowlist, worker_config=worker_config, audit_path=audit)

    assert completed.returncode == 2, completed.stderr + completed.stdout
    blob = completed.stderr + completed.stdout
    assert "worker_credential_disclosure" in blob, blob
    # The credential VALUE never appears in the diagnostic.
    assert CRED_VALUE not in blob
    # No model request (so no probe request) reached the wire.
    assert posts == [], posts
    # No worker was started (its env-dump record was never written) and none survives.
    assert not record.exists(), "a worker started despite the disclosure refusal"
    assert _worker_survivors(str(script_py)) == []
    # Refused before the audit opened: no audit file exists (not a lone run_start).
    assert not audit.exists(), "an audit file was opened before the config refusal"
