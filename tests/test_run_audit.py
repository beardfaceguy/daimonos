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
# A distinctive tool-result/file-body marker. R3 (ADR-026 Confirmation) scans the
# raw audit stream and asserts this exact string appears NOWHERE (results are
# stored only as SHA-256, never cleartext).
SEED_BODY_MARKER = "SEED_BODY_MARKER_7Q2XK9"


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
                    elif step[0] == "maxtokens":
                        # OpenAI/openrouter truncation finish_reason -> MaxTokens.
                        chunk({"role": "assistant", "content": step[1]}, "max_tokens")
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


def test_analytics_off_preserves_local_audit(daimonos_binary, tmp_path):
    provider = _FakeProvider([("end", DONE)])
    try:
        completed, audit_path = _run(daimonos_binary, tmp_path, provider, "say hello",
                                     extra_global=["--analytics-off"])
    finally:
        provider.close()
    assert completed.returncode == 0, completed.stderr
    events = _read_events(audit_path)
    assert (events[0]["kind"], events[-1]["kind"]) == ("run_start", "run_end")
    assert events[-1]["status"] == "ok"
    assert not list((tmp_path / "state").rglob("analytics.db*"))


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

    # R6: the audit file is created private (owner-only 0600) — never group- or
    # world-readable — so a run's own evidence record cannot leak via mode.
    import stat as _stat
    mode = _stat.S_IMODE(audit_path.stat().st_mode)
    assert mode == 0o600, oct(mode)

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
    (workspace / "seed.txt").write_text(f"evidence body {SEED_BODY_MARKER} must not be hashed in cleartext\n")

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
    # R6: the route is the exact native-tool identity, and a tool call that does
    # observable work records a nonzero elapsed duration (not a hardcoded/0).
    assert isinstance(rc["duration_ms"], int) and rc["duration_ms"] >= 1, rc
    assert rc["route"] == "native", rc

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

    # R3 (ADR-026 Confirmation): the file-body marker read by read_file must not
    # appear anywhere in the raw audit stream — not in any event or diagnostic.
    raw = Path(audit_path).read_text()
    assert SEED_BODY_MARKER not in raw, "tool-result content leaked into the audit record"


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

    # R2: run_start records the effective policy exactly as configured — the
    # tool allowlist from the agent env and the evidence root — so removing
    # either recording would fail here (not merely a key-presence check).
    assert events[0]["kind"] == "run_start", events[0]
    assert events[0]["effective_allowlist"] == ["read_file", "write_file", "ls"], events[0]
    assert events[0]["effective_root"] == str(tmp_path / "evidence"), events[0]

    model_requests = _by_kind(events, "model_request")
    assert model_requests, f"no model_request events: {events}"
    assert model_requests[0]["request_sequence"] == 0, model_requests[0]
    assert model_requests[0]["probe"] is False, model_requests[0]
    assert "http_status" in model_requests[0], model_requests[0]
    # R2: the endpoint-reported token usage is recorded against the configured
    # fake provider's counts (prompt_tokens=42, completion_tokens=7).
    assert model_requests[0]["usage"] == {"input_tokens": 42, "output_tokens": 7}, model_requests[0]
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


# --- R1: every frontend that accepts --audit-path must honor it or refuse it ---
# (ADR-026: a run must never falsely look audited.) Only `agent --print` writes
# the audit; chat, ACP, the MCP server, the session daemon, the interactive
# agent, and dry-run refuse the option at startup with a named error.

import signal
import os as _os


def _spawn_collect(cmd, env, *, timeout=15, pty_io=False):
    """Run cmd; return (returncode_or_None, stderr_text).

    Kills the whole process group on timeout so a frontend that fails to refuse
    (e.g. a daemon that would otherwise serve forever, or an interactive TUI)
    cannot linger. With pty_io, stdin+stdout are a pseudo-tty so is_terminal()
    is true (needed to force the interactive agent mode); stderr stays a pipe.
    """
    master = slave = None
    if pty_io:
        import pty as _ptylib
        master, slave = _ptylib.openpty()
        stdin_arg = stdout_arg = slave
    else:
        stdin_arg = subprocess.DEVNULL
        stdout_arg = subprocess.DEVNULL
    proc = subprocess.Popen(cmd, env=env, stdin=stdin_arg, stdout=stdout_arg,
                            stderr=subprocess.PIPE, start_new_session=True)
    if slave is not None:
        _os.close(slave)
    try:
        _, stderr = proc.communicate(timeout=timeout)
        rc = proc.returncode
    except subprocess.TimeoutExpired:
        try:
            _os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            _, stderr = proc.communicate(timeout=5)
        except Exception:
            stderr = b""
        rc = None
    finally:
        if master is not None:
            try:
                _os.close(master)
            except OSError:
                pass
    return rc, (stderr or b"").decode(errors="replace")


def _frontend_base(tmp_path, provider):
    """Common isolated env + a valid under-state audit path for a frontend run."""
    workspace = tmp_path / "evidence"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    audit_path = state / "audit" / "run.jsonl"
    env = _isolated_env(tmp_path)
    agent_env = _agent_env(tmp_path, provider)
    globals_ = [str(workspace), str(state), str(audit_path), str(agent_env)]
    return workspace, state, audit_path, env, agent_env


def _assert_refused(rc, stderr, audit_path):
    assert rc == 2, (rc, stderr)
    assert "run_configuration_invalid" in stderr, stderr
    assert "audit_unsupported_frontend" in stderr, stderr
    assert "--audit-path" in stderr, stderr
    assert FAKE_KEY not in stderr
    assert not audit_path.exists(), "a refused frontend must not create the audit file"


def test_chat_frontend_refuses_audit_path(daimonos_binary, tmp_path):
    provider = _FakeProvider([("end", DONE)])
    try:
        _, state, audit_path, env, agent_env = _frontend_base(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(tmp_path / "evidence"),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "chat", "--agent-env", str(agent_env)]
        rc, stderr = _spawn_collect(cmd, env)
    finally:
        provider.close()
    _assert_refused(rc, stderr, audit_path)


def test_acp_frontend_refuses_audit_path(daimonos_binary, tmp_path):
    provider = _FakeProvider([("end", DONE)])
    try:
        _, state, audit_path, env, agent_env = _frontend_base(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(tmp_path / "evidence"),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "acp", "--agent-env", str(agent_env)]
        rc, stderr = _spawn_collect(cmd, env)
    finally:
        provider.close()
    _assert_refused(rc, stderr, audit_path)


def test_mcp_server_refuses_audit_path(daimonos_binary, tmp_path):
    provider = _FakeProvider([("end", DONE)])
    try:
        _, state, audit_path, env, agent_env = _frontend_base(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(tmp_path / "evidence"),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "mcp"]
        rc, stderr = _spawn_collect(cmd, env)
    finally:
        provider.close()
    _assert_refused(rc, stderr, audit_path)


def test_session_daemon_refuses_audit_path(daimonos_binary, tmp_path):
    provider = _FakeProvider([("end", DONE)])
    try:
        _, state, audit_path, env, agent_env = _frontend_base(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(tmp_path / "evidence"),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "session-daemon", "--agent-env", str(agent_env)]
        rc, stderr = _spawn_collect(cmd, env)
    finally:
        provider.close()
    _assert_refused(rc, stderr, audit_path)


def test_interactive_agent_refuses_audit_path(daimonos_binary, tmp_path):
    """The interactive agent (a TTY session, attached to the session daemon)
    does not write the audit in-process, so it refuses the option at startup —
    before any daemon launch — rather than run a falsely-audited session."""
    provider = _FakeProvider([("end", DONE)])
    try:
        _, state, audit_path, env, agent_env = _frontend_base(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(tmp_path / "evidence"),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "agent", "--interactive", "--agent-env", str(agent_env), "hi"]
        rc, stderr = _spawn_collect(cmd, env, pty_io=True, timeout=15)
    finally:
        provider.close()
    _assert_refused(rc, stderr, audit_path)


def test_dry_run_refuses_audit_path(daimonos_binary, tmp_path):
    """R3: `agent --dry-run` is a pure inspection mode that writes no audit, so
    it refuses --audit-path at startup (exit 2, audit_unsupported_frontend) and
    creates no file — a dry run must never look audited."""
    provider = _FakeProvider([("end", DONE)])
    try:
        _, state, audit_path, env, agent_env = _frontend_base(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(tmp_path / "evidence"),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "agent", "--dry-run", "--agent-env", str(agent_env), "hi"]
        rc, stderr = _spawn_collect(cmd, env)
    finally:
        provider.close()
    _assert_refused(rc, stderr, audit_path)
    # No model request runs either: refusal precedes any provider contact.
    assert provider.post_count == 0, provider.post_count


def _write_config(tmp_path, body):
    path = tmp_path / "daimonos.toml"
    path.write_text(body)
    return path


def test_audit_mid_run_write_failure_stops_the_run(daimonos_binary, tmp_path):
    """R2 (ADR-026 Confirmation): a mid-run audit write failure (forced with a
    tiny max_events so the first model_request trips the count limit) aborts the
    run with exit 23, emits no completion marker, and runs no model request
    after the failure."""
    config = _write_config(tmp_path, "[reviewer.audit]\nmax_events = 1\n")
    provider = _FakeProvider([("end", DONE), ("end", DONE), ("end", DONE)])
    try:
        completed, audit_path = _run(daimonos_binary, tmp_path, provider, "hi",
                                     extra_global=["--config", str(config)])
    finally:
        provider.close()

    assert completed.returncode == 23, (completed.returncode, completed.stderr)
    assert "audit_write_failed" in completed.stderr, completed.stderr
    assert DONE not in completed.stdout, "a completion marker survived a mid-run audit failure"
    # run_start succeeded (one line); nothing after the failing request ran.
    assert provider.post_count == 1, provider.post_count
    assert FAKE_KEY not in completed.stderr


def test_audit_finish_failure_is_fail_closed(daimonos_binary, tmp_path):
    """R2 (ADR-026 Confirmation): a finish-time (terminal run_end) audit write
    failure — forced with max_events=2 so run_start + one model_request succeed
    but run_end trips the limit — fails the run with exit 23 and does NOT publish
    the assistant output, proving the terminal record is written before success."""
    config = _write_config(tmp_path, "[reviewer.audit]\nmax_events = 2\n")
    provider = _FakeProvider([("end", DONE)])
    try:
        completed, audit_path = _run(daimonos_binary, tmp_path, provider, "hi",
                                     extra_global=["--config", str(config)])
    finally:
        provider.close()

    assert completed.returncode == 23, (completed.returncode, completed.stderr)
    assert "audit_write_failed" in completed.stderr, completed.stderr
    assert DONE not in completed.stdout, "assistant output was published before the terminal audit event"
    assert provider.post_count == 1, provider.post_count
    events = _read_events(audit_path)
    assert events[-1]["kind"] != "run_end", events
    assert FAKE_KEY not in completed.stderr


def test_audit_path_inside_workspace_without_state_dir_is_refused(daimonos_binary, tmp_path):
    """R4: without an explicit state root, an audit destination inside the
    evidence workspace is refused at startup (exit 2, run_configuration_invalid,
    reason audit_path_inside_workspace) and no model request runs."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    inside = workspace / "audit.jsonl"
    env = _isolated_env(tmp_path)
    provider = _FakeProvider([("end", DONE)])
    try:
        agent_env = _agent_env(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--audit-path", str(inside),
               "agent", "--print", "--agent-env", str(agent_env), "hi"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=60, env=env)
    finally:
        provider.close()

    assert completed.returncode == 2, (completed.returncode, completed.stderr)
    assert "run_configuration_invalid" in completed.stderr, completed.stderr
    assert "audit_path_inside_workspace" in completed.stderr, completed.stderr
    assert provider.post_count == 0
    assert not inside.exists()


def test_run_end_status_agrees_with_process_exit_on_non_endturn(daimonos_binary, tmp_path):
    """R7: for a run that ends without EndTurn (here a max_tokens truncation with
    auto-continue off), the run_end status/exit are derived from the same
    terminal source as the process exit, so the recorded exit_code equals the
    process return code and the status reflects the real (non-'ok') ending."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
    env = _isolated_env(tmp_path)
    env["DAIMONOS_AGENT_AUTO_CONTINUE"] = "0"
    provider = _FakeProvider([("maxtokens", "truncated partial answer")])
    try:
        agent_env = _agent_env(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "agent", "--print", "--agent-env", str(agent_env), "hi"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=60, env=env)
    finally:
        provider.close()

    events = _read_events(audit_path)
    run_end = events[-1]
    assert run_end["kind"] == "run_end", events
    assert run_end["status"] == "max_tokens", run_end
    assert run_end["exit_code"] == completed.returncode, (run_end, completed.returncode)
    assert completed.returncode == 0, completed.stderr
