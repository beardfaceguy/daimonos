"""WP08 — local, privacy-preserving run audit (FR-04 reqs 18-20, ADR-026).

These acceptance tests drive a real `daimonos agent --print` process against a
local, self-contained fake OpenAI-compatible endpoint (no network, no real
credential) and assert on the caller-requested JSONL audit record written to
`--audit-path`.

Context: ADR-026 (caller-requested local audit; fail-closed required-write
semantics, distinct from ADR-006 OTLP fail-open), reviewer-runtime
INTERFACES.md (Audit/output contracts, exit 23 `audit_write_failed`).
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# A recognizable fake credential. AC-04.8 greps the whole audit record for this
# exact value and must find nothing.
FAKE_KEY = "synthetic-not-a-real-key-AUDITSCAN-5fZ9"
DONE = "AUDIT_AGENT_DONE_MARKER"


class _FakeProvider:
    """Local fake OpenAI-compatible (`openrouter`) endpoint.

    `script` controls the per-turn behavior. Each POST advances one step:
    a step is either ("end", text) — finish with assistant text — or
    ("call", name, arguments) — ask the model to call a tool. After a tool
    result comes back the next step runs.
    """

    def __init__(self, script):
        self._script = list(script)
        self._step = 0
        self.post_count = 0
        self.received_tool_results = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{"id": "test/model",
                    "context_length": 100000, "supported_parameters": ["tools"],
                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                owner.post_count += 1
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                tool_msgs = [m for m in body["messages"] if m.get("role") == "tool"]
                if tool_msgs:
                    owner.received_tool_results.append(json.dumps(tool_msgs))
                step = owner._script[min(owner._step, len(owner._script) - 1)]
                owner._step += 1
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

                def chunk(delta, reason=None):
                    frame = {"choices": [{"index": 0, "delta": delta,
                        "finish_reason": reason}],
                        "usage": {"prompt_tokens": 42, "completion_tokens": 7}}
                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                    self.wfile.flush()

                try:
                    if step[0] == "call":
                        _, name, arguments = step
                        chunk({"role": "assistant", "content": "", "tool_calls": [{
                            "index": 0, "id": f"call-{owner._step}", "type": "function",
                            "function": {"name": name,
                                         "arguments": json.dumps(arguments)}}]},
                              "tool_calls")
                    else:
                        chunk({"role": "assistant", "content": step[1]}, "stop")
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


def _isolated_env(tmp_path):
    stash = tmp_path / "home"
    stash.mkdir(exist_ok=True)
    (tmp_path / "tmp").mkdir(exist_ok=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith("DAIMONOS_")}
    env.update({
        "HOME": str(stash),
        "XDG_STATE_HOME": str(stash / "state"),
        "XDG_DATA_HOME": str(stash / "data"),
        "XDG_CONFIG_HOME": str(stash / "config"),
        "TMPDIR": str(tmp_path / "tmp"),
        "DAIMONOS_KGL_AUTOINDEX": "0",
    })
    return env


def _agent_env(tmp_path, provider, allowed="read_file,write_file,ls"):
    path = tmp_path / "agent.env"
    path.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.port}/v1",
        f"DAIMONOS_AGENT_API_KEY={FAKE_KEY}",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        f"DAIMONOS_AGENT_ALLOWED_COMMANDS={allowed}",
        "",
    ]))
    return path


def _run(daimonos_binary, tmp_path, provider, task, *, extra_global=None,
         audit_rel="state/audit/run.jsonl", allowed="read_file,write_file,ls",
         make_state=True, timeout=60):
    """Spawn `daimonos agent --print` with the fake provider and --audit-path.

    Returns (completed_process, audit_path).
    """
    workspace = tmp_path / "evidence"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    if make_state:
        state.mkdir(mode=0o700, exist_ok=True)
    audit_path = tmp_path / audit_rel
    env = _isolated_env(tmp_path)
    agent_env = _agent_env(tmp_path, provider, allowed=allowed)
    cmd = [daimonos_binary, "--workspace", str(workspace),
           "--state-dir", str(state), "--audit-path", str(audit_path),
           *(extra_global or []),
           "agent", "--print", "--agent-env", str(agent_env), task]
    completed = subprocess.run(cmd, capture_output=True, text=True,
                               timeout=timeout, env=env)
    return completed, audit_path


def _read_events(audit_path):
    lines = Path(audit_path).read_text().splitlines()
    return [json.loads(line) for line in lines if line.strip()]


def test_successful_run_writes_run_start_and_run_end(daimonos_binary, tmp_path):
    """Slice 1: an opt-in run writes a versioned JSONL record whose first event
    is `run_start` and whose terminal event is `run_end` with the final status
    and exit code. Every event shares one run_id and a monotonic sequence."""
    provider = _FakeProvider([("end", DONE)])
    try:
        completed, audit_path = _run(daimonos_binary, tmp_path, provider,
                                     "say hello")
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stderr
    assert DONE in completed.stdout, completed.stdout
    assert audit_path.exists(), "audit record was not written"

    events = _read_events(audit_path)
    assert len(events) >= 2, events
    for i, ev in enumerate(events):
        assert ev["kind"], ev
        assert ev["audit_version"], ev
        assert ev["sequence"] == i, (i, ev)
    run_id = events[0]["run_id"]
    assert run_id, events[0]
    assert all(ev["run_id"] == run_id for ev in events)

    assert events[0]["kind"] == "run_start", events[0]
    assert events[-1]["kind"] == "run_end", events[-1]
    assert events[-1]["status"] == "ok", events[-1]
    assert events[-1]["exit_code"] == 0, events[-1]


import re

_HEX64 = re.compile(r"^[0-9a-f]{64}$")

# Tool refusal codes (reviewer-runtime INTERFACES.md). A `tool_refusal` event's
# `reason` is one of these; a `tool_call` event is an allowed execution.
_REFUSAL_CODES = {
    "tool_not_allowed", "path_outside_root", "workspace_write_forbidden",
    "symlink_refused", "tool_policy_unsupported", "tool_timeout",
    "tool_input_limit", "offline_tool_forbidden",
}


def _by_kind(events, kind):
    return [e for e in events if e["kind"] == kind]


def test_tool_call_and_refusal_records(daimonos_binary, tmp_path):
    """Slice 2: each dispatched tool is recorded at the boundary. An allowed
    read is a `tool_call`; a refused mutation is a `tool_refusal` naming its
    reason. Both carry the exported name, a route, SHA-256 of the argument and
    the result (never contents), and an elapsed duration."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "seed.txt").write_text("evidence body that must not be hashed in cleartext\n")

    provider = _FakeProvider([
        ("call", "read_file", {"path": "seed.txt"}),
        ("call", "write_file", {"path": "out.txt", "content": "nope"}),
        ("end", DONE),
    ])
    try:
        completed, audit_path = _run(
            daimonos_binary, tmp_path, provider, "inspect then try to write",
            extra_global=["--workspace-read-only"],
        )
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stderr
    events = _read_events(audit_path)

    calls = _by_kind(events, "tool_call")
    refusals = _by_kind(events, "tool_refusal")

    read_calls = [e for e in calls if e["name"] == "read_file"]
    assert read_calls, f"no tool_call for read_file: {events}"
    rc = read_calls[0]
    assert rc["allowed"] is True, rc
    assert _HEX64.match(rc["argument_sha256"]), rc
    assert _HEX64.match(rc["result_sha256"]), rc
    assert isinstance(rc["duration_ms"], int) and rc["duration_ms"] >= 0, rc
    assert rc["route"], rc

    write_refusals = [e for e in refusals if e["name"] == "write_file"]
    assert write_refusals, f"no tool_refusal for write_file: {events}"
    wr = write_refusals[0]
    assert wr["allowed"] is False, wr
    assert wr["reason"] == "workspace_write_forbidden", wr
    assert _HEX64.match(wr["argument_sha256"]), wr
    assert _HEX64.match(wr["result_sha256"]), wr

    # The recorded argument hash equals SHA-256 of the canonical argument JSON,
    # proving the hash is of the bytes at the boundary (not a Debug string) and
    # that no cleartext argument was stored.
    import hashlib
    canonical = json.dumps({"path": "seed.txt"}, separators=(",", ":"), sort_keys=True)
    assert rc["argument_sha256"] == hashlib.sha256(canonical.encode()).hexdigest(), rc


def test_model_requests_recorded_and_no_credential_in_record(daimonos_binary, tmp_path):
    """Slice 3 (AC-04.8 + event completeness): a run records its model requests
    (sequence, probe flag, reported usage) and the complete audit record never
    contains the credential VALUE. The credential VARIABLE NAME is recorded, in
    its place, proving redaction records the name not the secret."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "seed.txt").write_text("evidence body\n")

    provider = _FakeProvider([
        ("call", "read_file", {"path": "seed.txt"}),
        ("call", "write_file", {"path": "out.txt", "content": "nope"}),
        ("end", DONE),
    ])
    try:
        completed, audit_path = _run(
            daimonos_binary, tmp_path, provider, "inspect",
            extra_global=["--workspace-read-only"],
        )
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stderr
    raw = Path(audit_path).read_text()
    events = _read_events(audit_path)

    model_requests = _by_kind(events, "model_request")
    assert model_requests, f"no model_request events: {events}"
    assert model_requests[0]["request_sequence"] == 0, model_requests[0]
    assert model_requests[0]["probe"] is False, model_requests[0]
    assert "http_status" in model_requests[0], model_requests[0]
    # sequences are contiguous per model request
    seqs = [m["request_sequence"] for m in model_requests]
    assert seqs == list(range(len(seqs))), seqs

    # AC-04.8: the exact credential value appears NOWHERE in the record.
    assert FAKE_KEY not in raw, "credential value leaked into the audit record"
    # The credential variable NAME is what gets recorded.
    assert events[0]["credential_env"] == "DAIMONOS_AGENT_API_KEY", events[0]
    # And the full base URL (which could carry userinfo/path) is never recorded;
    # only its host is.
    assert "/v1" not in raw, "base URL path leaked into the audit record"
    assert events[0]["base_url_host"] == "127.0.0.1", events[0]


def test_audit_open_failure_stops_run_before_any_model_request(daimonos_binary, tmp_path):
    """Slice 4 (ADR-026 fail-closed): when the requested audit cannot be opened
    (here: the destination already exists, so no silent append), the run fails
    with exit 23 `audit_write_failed` before any model request, and produces no
    successful output. This is distinct from OTLP's fail-open policy."""
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    audit_path = state / "audit" / "run.jsonl"
    audit_path.parent.mkdir(parents=True)
    audit_path.write_text('{"pre":"existing run — must not be appended"}\n')

    provider = _FakeProvider([("end", DONE)])
    try:
        completed, _ = _run(daimonos_binary, tmp_path, provider, "hi",
                            make_state=False)
    finally:
        provider.close()

    assert completed.returncode == 23, (completed.returncode, completed.stderr)
    assert "audit_write_failed" in completed.stderr, completed.stderr
    assert DONE not in completed.stdout, "run produced output despite audit failure"
    # No model request was made: the sink opens before the provider loop.
    assert provider.post_count == 0, "a model request ran despite the audit open failure"
    # The pre-existing file was not appended to.
    assert audit_path.read_text().splitlines() == ['{"pre":"existing run — must not be appended"}']
    # The diagnostic never echoes a secret.
    assert FAKE_KEY not in completed.stderr


def test_audit_path_outside_state_root_is_refused_naming_the_option(daimonos_binary, tmp_path):
    """Slice 5 (PM FR-08 / INTERFACES): with an explicit state root the audit
    path must be under it. An outside path fails startup (exit 2,
    run_configuration_invalid) and the error names the offending option and the
    path. No model request runs."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside" / "audit.jsonl"
    env = _isolated_env(tmp_path)
    provider = _FakeProvider([("end", DONE)])
    try:
        agent_env = _agent_env(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--state-dir", str(state), "--audit-path", str(outside),
               "agent", "--print", "--agent-env", str(agent_env), "hi"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=60, env=env)
    finally:
        provider.close()

    assert completed.returncode == 2, (completed.returncode, completed.stderr)
    assert "run_configuration_invalid" in completed.stderr, completed.stderr
    assert "--audit-path" in completed.stderr, completed.stderr
    assert str(outside) in completed.stderr, completed.stderr
    assert provider.post_count == 0
    assert not outside.exists()


def test_opt_out_leaves_behavior_unchanged(daimonos_binary, tmp_path):
    """Slice 6 (opt-out regression): without --audit-path no audit file is
    written and the run behaves exactly as before (exit 0, assistant output)."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = _isolated_env(tmp_path)
    provider = _FakeProvider([("end", DONE)])
    try:
        agent_env = _agent_env(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--state-dir", str(state),
               "agent", "--print", "--agent-env", str(agent_env), "hi"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=60, env=env)
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stderr
    assert DONE in completed.stdout, completed.stdout
    # No stray audit artifact anywhere under the state root.
    produced = list(state.rglob("*.jsonl"))
    assert produced == [], f"audit file written with the option off: {produced}"
