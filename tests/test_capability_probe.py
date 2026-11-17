"""WP09 (FR-04 requirements 14-17, ADR-027): the same-tool capability probe.

These exercise the probe through the REAL `daimonos agent --print` process. A
single unretried probe request is sent against a local fake model endpoint
before any evidence tool runs; only one valid final-answer call passes, and
every other outcome (text-only, wrong/multiple calls, invalid arguments, an
endpoint 503, or a 3xx redirect) yields the distinct `model_unsupported` exit
(21) with no further work. No live credentialed endpoint is ever contacted.

The probe requires FR-01 structured final output (`--final-output`) and a
complete controlled model profile (`--model-profile`) (PM answer A-to-Q-FR-04-2
and reviewer-runtime INTERFACES.md); omitting either is a startup refusal.

Fake-endpoint pattern follows tests/test_request_control.py.
"""

import json
import os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TIMEOUT = 60

# The credential env var every controlled profile below names. Its VALUE must
# never reach the recorded request or the audit record.
CRED_ENV = "DBUNK_PROBE_KEY"
CRED_VALUE = "sk-probe-PROCESS-SECRET-9z8y7x"

# The caller's designated final-answer tool and its minimal valid schema.
FINAL_TOOL = "final_answer"
FINAL_SCHEMA = {
    "type": "object",
    "properties": {"verdict": {"type": "string"}},
    "required": ["verdict"],
    "additionalProperties": False,
}
# A minimal valid payload for the probe (validates against FINAL_SCHEMA).
PROBE_PAYLOAD = {"verdict": "supported"}


def _write_final_output(path, max_model_requests=None):
    control = {
        "schema": FINAL_SCHEMA,
        "tool_name": FINAL_TOOL,
        "tool_description": "Emit the final verdict.",
        "corrective_retries": 0,
    }
    if max_model_requests is not None:
        control["max_model_requests"] = max_model_requests
    path.write_text(json.dumps(control))
    return path


def _write_profile(path, base_url, wire="messages", request_retries=0,
                   provider_label="anthropic", model="claude-opus-5-5"):
    path.write_text(json.dumps({
        "provider_label": provider_label,
        "model": model,
        "base_url": base_url,
        "wire": wire,
        "credential_env": CRED_ENV,
        "request_retries": request_retries,
        "max_tokens": 256,
    }))
    return path


def _write_payload(path, payload=None):
    path.write_text(json.dumps(PROBE_PAYLOAD if payload is None else payload))
    return path


def _env(tmp_path, agent_base="http://127.0.0.1:1", extra_env=None):
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=unused/model",
        f"DAIMONOS_AGENT_BASE_URL={agent_base}",
        "DAIMONOS_AGENT_API_KEY=unused-agent-key",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        "DAIMONOS_AGENT_CONTEXT_WINDOW=100000",
        "",
    ]))
    env = {k: v for k, v in os.environ.items() if not k.startswith("DAIMONOS_")}
    env.update(
        HOME=str(home),
        XDG_CONFIG_HOME=str(home / ".config"),
        XDG_DATA_HOME=str(home / "data"),
        TMPDIR=str(tmp_path),
    )
    env[CRED_ENV] = CRED_VALUE
    if extra_env:
        env.update(extra_env)
    return agent_env, env


def _run_probe(daimonos_binary, tmp_path, *, endpoint_url, wire="messages",
               request_retries=0, max_model_requests=None, payload=None,
               with_final=True, with_profile=True, audit_path=None,
               mode="print", extra_argv=()):
    workspace = tmp_path / "workspace"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    final_f = _write_final_output(tmp_path / "final.json", max_model_requests)
    profile_f = _write_profile(tmp_path / "profile.json", endpoint_url, wire,
                               request_retries)
    payload_f = _write_payload(tmp_path / "payload.json", payload)
    agent_env, env = _env(tmp_path)
    argv = [daimonos_binary, "--workspace", str(workspace),
            "--state-dir", str(state)]
    if audit_path is not None:
        argv += ["--audit-path", str(audit_path)]
    argv += ["agent"]
    if mode == "print":
        argv += ["--print"]
    elif mode == "dry-run":
        argv += ["--dry-run"]
    argv += ["--agent-env", str(agent_env)]
    if with_profile:
        argv += ["--model-profile", str(profile_f)]
    if with_final:
        argv += ["--final-output", str(final_f)]
    argv += ["--capability-probe-payload", str(payload_f)]
    argv += list(extra_argv)
    argv += ["review the evidence"]
    return subprocess.run(argv, capture_output=True, text=True,
                          timeout=TIMEOUT, env=env)


# --------------------------------------------------------------------------
# Slice 01: the caller interface and its startup refusals.
# --------------------------------------------------------------------------

def test_agent_help_lists_capability_probe_payload(daimonos_binary):
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True, text=True, timeout=TIMEOUT,
    )
    assert "--capability-probe-payload" in completed.stdout


def test_probe_requires_final_output(daimonos_binary, tmp_path):
    # Probe without --final-output (FR-01) fails at startup (PM A-to-Q-FR-04-2).
    result = _run_probe(daimonos_binary, tmp_path,
                        endpoint_url="http://127.0.0.1:1", with_final=False)
    assert result.returncode == 2, result.stderr + result.stdout
    blob = result.stderr + result.stdout
    assert "capability-probe-payload" in blob
    assert "final-output" in blob


def test_probe_requires_model_profile(daimonos_binary, tmp_path):
    # Probe without --model-profile (controlled transport) fails at startup.
    result = _run_probe(daimonos_binary, tmp_path,
                        endpoint_url="http://127.0.0.1:1", with_profile=False)
    assert result.returncode == 2, result.stderr + result.stdout
    blob = result.stderr + result.stdout
    assert "capability-probe-payload" in blob
    assert "model-profile" in blob


def test_probe_refused_in_dry_run(daimonos_binary, tmp_path):
    # Dry-run is a frontend that accepts the agent flag but cannot send a probe
    # (no provider contact); it must be refused with a named error, never
    # silently accepted. --final-output already conflicts with --dry-run at
    # parse, so the combination fails closed at startup.
    result = _run_probe(daimonos_binary, tmp_path,
                        endpoint_url="http://127.0.0.1:1", mode="dry-run")
    assert result.returncode == 2, result.stderr + result.stdout


def test_probe_flag_rejected_by_non_agent_frontends(daimonos_binary, tmp_path):
    # The probe is an agent-only flag; chat/acp/session-daemon never carry it
    # and reject it as an unexpected argument (fail-closed, not ignored).
    payload_f = _write_payload(tmp_path / "payload.json")
    for sub in ("chat", "acp", "session-daemon"):
        result = subprocess.run(
            [daimonos_binary, sub, "--capability-probe-payload", str(payload_f)],
            capture_output=True, text=True, timeout=TIMEOUT,
        )
        assert result.returncode != 0, f"{sub} accepted the probe flag"
        blob = (result.stderr + result.stdout).lower()
        assert "capability-probe-payload" in blob or "unexpected" in blob, \
            f"{sub}: {blob}"


# --------------------------------------------------------------------------
# Slice 02: the caller's minimal payload is validated LOCALLY before the
# probe is ever sent (ADR-027: "Validate the caller's minimal probe payload
# locally before sending anything"). An invalid payload is a caller
# configuration error (exit 2), not a model failure, and no request is sent.
# --------------------------------------------------------------------------

class _UnusedEndpoint:
    """Records any connection so a test can assert the probe never sent one."""

    def __init__(self):
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def _handle(self, method):
                owner.requests.append(method)
                payload = b"{}"
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                self._handle("POST")

            def do_GET(self):
                self._handle("GET")

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


def test_payload_failing_schema_is_refused_locally_before_any_request(daimonos_binary, tmp_path):
    endpoint = _UnusedEndpoint()
    try:
        # verdict must be a string; 42 fails the schema.
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            payload={"verdict": 42})
    finally:
        endpoint.close()
    assert result.returncode == 2, result.stderr + result.stdout
    assert "capability-probe-payload" in (result.stderr + result.stdout)
    assert endpoint.requests == [], "no request may be sent for an invalid payload"


def test_payload_missing_required_field_is_refused_locally(daimonos_binary, tmp_path):
    endpoint = _UnusedEndpoint()
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            payload={})
    finally:
        endpoint.close()
    assert result.returncode == 2, result.stderr + result.stdout
    assert endpoint.requests == [], "no request may be sent for an invalid payload"


def test_payload_file_that_is_not_json_is_refused(daimonos_binary, tmp_path):
    endpoint = _UnusedEndpoint()
    try:
        # Write a non-JSON payload file after _run_probe would have written a
        # valid one, so overwrite via extra handling: pass a payload object that
        # json.dumps cannot be parsed back wrongly -> instead point the flag at
        # a hand-written bad file.
        workspace = tmp_path / "workspace"
        workspace.mkdir(exist_ok=True)
        state = tmp_path / "state"
        state.mkdir(mode=0o700, exist_ok=True)
        final_f = _write_final_output(tmp_path / "final.json")
        profile_f = _write_profile(tmp_path / "profile.json", endpoint.url)
        bad = tmp_path / "bad_payload.json"
        bad.write_text("this is not json {")
        agent_env, env = _env(tmp_path)
        result = subprocess.run(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "agent", "--print", "--agent-env", str(agent_env),
             "--model-profile", str(profile_f), "--final-output", str(final_f),
             "--capability-probe-payload", str(bad), "review the evidence"],
            capture_output=True, text=True, timeout=TIMEOUT, env=env,
        )
    finally:
        endpoint.close()
    assert result.returncode == 2, result.stderr + result.stdout
    assert "capability-probe-payload" in (result.stderr + result.stdout)
    assert endpoint.requests == [], "no request may be sent for an unreadable payload"


# --------------------------------------------------------------------------
# Slice 03: the probe runs against a local fake model endpoint before any
# evidence tool. Only one schema-valid final-answer call passes; every other
# outcome yields exit 21 (`model_unsupported`) with exactly one request sent
# and no further work (ADR-027, FR-04 reqs 14-17, AC-04.3/04.4/04.6).
# --------------------------------------------------------------------------

import pytest

_FINAL_OK = {"content": [{"type": "tool_use", "id": "t1", "name": FINAL_TOOL,
                          "input": {"verdict": "supported"}}],
             "stop_reason": "tool_use",
             "usage": {"input_tokens": 1, "output_tokens": 1}}
_TEXT_ONLY = {"content": [{"type": "text", "text": "I believe so"}],
              "stop_reason": "end_turn",
              "usage": {"input_tokens": 1, "output_tokens": 1}}
_WRONG_TOOL = {"content": [{"type": "tool_use", "id": "t1", "name": "read_file",
                            "input": {"path": "a.txt"}}],
               "stop_reason": "tool_use",
               "usage": {"input_tokens": 1, "output_tokens": 1}}
_TWO_CALLS = {"content": [
    {"type": "tool_use", "id": "t1", "name": FINAL_TOOL, "input": {"verdict": "y"}},
    {"type": "tool_use", "id": "t2", "name": "read_file", "input": {"path": "a"}}],
    "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}}
_INVALID_ARGS = {"content": [{"type": "tool_use", "id": "t1", "name": FINAL_TOOL,
                              "input": {"verdict": 42}}],
                 "stop_reason": "tool_use",
                 "usage": {"input_tokens": 1, "output_tokens": 1}}
# A text (reasoning) block ALONGSIDE the single valid final call (R3, req 16).
_TEXT_PLUS_FINAL = {"content": [
    {"type": "text", "text": "Here is my verdict."},
    {"type": "tool_use", "id": "t1", "name": FINAL_TOOL,
     "input": {"verdict": "supported"}}],
    "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}}
# A valid final call whose verdict DIFFERS from the suggested probe payload (R7,
# ADR-027: no byte-for-byte equality with the suggested payload is demanded).
_FINAL_DIFFERENT = {"content": [{"type": "tool_use", "id": "t1", "name": FINAL_TOOL,
                                 "input": {"verdict": "a-different-valid-value"}}],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 1, "output_tokens": 1}}

_BODIES = {
    "final_ok": _FINAL_OK, "text_only": _TEXT_ONLY, "wrong_tool": _WRONG_TOOL,
    "two_calls": _TWO_CALLS, "invalid_args": _INVALID_ARGS,
    "text_plus_final": _TEXT_PLUS_FINAL, "final_different": _FINAL_DIFFERENT,
}


class ProbeEndpoint:
    """Local fake Messages endpoint that replies with a configurable assistant
    response (a tool call, text, etc.) or an error status, recording every
    request so a test can assert exactly one was sent."""

    def __init__(self, mode="final_ok", redirect_to=None):
        self.mode = mode
        self.redirect_to = redirect_to
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def _record(self, method):
                length = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = None
                owner.requests.append({"method": method, "path": self.path,
                                       "body": body})

            def _send(self, status, payload, extra_headers=None):
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                for k, v in (extra_headers or {}).items():
                    self.send_header(k, v)
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                self._record("POST")
                if owner.mode == "503":
                    self._send(503, b"{}")
                    return
                if owner.mode == "400":
                    self._send(400, b"{}")
                    return
                if owner.mode == "307":
                    self._send(307, b"{}",
                               {"Location": owner.redirect_to or "http://127.0.0.1:1/x"})
                    return
                body = _BODIES[owner.mode]
                self._send(200, json.dumps(body).encode())

            def do_GET(self):
                self._record("GET")
                self._send(200, b'{"data": []}')

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def url(self):
        host, port = self.server.server_address
        return f"http://{host}:{port}"

    @property
    def posts(self):
        return [r for r in self.requests if r["method"] == "POST"]

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self._thread.join(timeout=3)


def test_probe_pass_offers_frozen_catalog_and_run_proceeds(daimonos_binary, tmp_path):
    # One valid final-answer call passes; the run then proceeds and succeeds.
    endpoint = ProbeEndpoint("final_ok")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    posts = endpoint.posts
    assert len(posts) >= 1, posts
    probe_body = posts[0]["body"]
    assert posts[0]["path"] == "/v1/messages"
    tool_names = {t["name"] for t in probe_body["tools"]}
    assert FINAL_TOOL in tool_names, tool_names
    assert probe_body["tool_choice"] == {"type": "auto"}, probe_body["tool_choice"]
    # The successful run emits exactly the validated final payload on stdout.
    assert json.loads(result.stdout) == {"verdict": "supported"}, result.stdout


@pytest.mark.parametrize("mode,reason", [
    ("text_only", "text_only"),
    ("wrong_tool", "wrong_tool"),
    ("two_calls", "multiple_calls"),
    ("invalid_args", "invalid_arguments"),
])
def test_probe_failure_is_model_unsupported_with_no_further_work(
        daimonos_binary, tmp_path, mode, reason):
    endpoint = ProbeEndpoint(mode)
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    # Exactly one request: the probe. No evidence-tool / loop request follows.
    assert len(endpoint.posts) == 1, endpoint.posts
    blob = result.stderr + result.stdout
    assert "model_unsupported" in blob, blob
    assert reason in blob, blob


def test_probe_503_is_one_request_and_model_unsupported(daimonos_binary, tmp_path):
    # AC-04.3: a probe answered with HTTP 503 produces exactly one request and
    # the model_unsupported exit (no retry).
    endpoint = ProbeEndpoint("503")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    assert len(endpoint.posts) == 1, endpoint.posts


def test_probe_redirect_is_one_request_and_no_other_host(daimonos_binary, tmp_path):
    # AC-04.4: a 3xx answer is an endpoint error; the body is never re-sent to
    # the redirect target.
    other = _UnusedEndpoint()
    endpoint = ProbeEndpoint("307", redirect_to=other.url + "/v1/messages")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
        other.close()
    assert result.returncode == 21, result.stderr + result.stdout
    assert len(endpoint.posts) == 1, endpoint.posts
    assert other.requests == [], "redirect target must receive nothing"


def test_probe_is_one_attempt_even_when_profile_allows_retries(daimonos_binary, tmp_path):
    # ADR-027 / INTERFACES.md: the probe is exactly one HTTP attempt with no
    # retry, even when the run's controlled profile permits retries.
    endpoint = ProbeEndpoint("503")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            request_retries=3)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    assert len(endpoint.posts) == 1, endpoint.posts


# --------------------------------------------------------------------------
# Slice 04: the probe outcome is recorded in the local run audit (FR-04 reqs
# 18-19, AC-04.6), and the audit never contains the credential value (AC-04.8).
# --------------------------------------------------------------------------

def _read_events(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def test_probe_failure_outcome_is_recorded_in_the_audit(daimonos_binary, tmp_path):
    endpoint = ProbeEndpoint("text_only")
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    assert audit.exists(), "audit file was not written"
    events = _read_events(audit)
    probe_events = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe_events) == 1, events
    assert probe_events[0]["status"] == "model_unsupported", probe_events[0]
    assert probe_events[0]["reason"] == "text_only", probe_events[0]
    # AC-04.8: the credential value never appears in the audit record.
    assert CRED_VALUE not in audit.read_text()


def test_probe_pass_outcome_is_recorded_in_the_audit(daimonos_binary, tmp_path):
    endpoint = ProbeEndpoint("final_ok")
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    events = _read_events(audit)
    probe_events = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe_events) == 1, events
    assert probe_events[0]["status"] == "model_supported", probe_events[0]
    assert CRED_VALUE not in audit.read_text()


# --------------------------------------------------------------------------
# Slice 05 (R5, reviewer-runtime INTERFACES.md): the single probe request
# counts against the WP05 inclusive model-request ceiling. With
# max_model_requests=1 the probe consumes the one permitted request, so the
# agent loop's first request is refused before it is sent: the run exits 24
# (model_request_limit, reason `turn_limit`) with exactly one request on the
# wire (the probe) and no evidence-tool work. A probe that is NOT counted
# would leave the loop a request and let the run reach exit 0 with two POSTs.
# --------------------------------------------------------------------------

def test_probe_request_counts_against_model_request_ceiling(daimonos_binary, tmp_path):
    endpoint = ProbeEndpoint("final_ok")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            max_model_requests=1)
    finally:
        endpoint.close()
    assert result.returncode == 24, result.stderr + result.stdout
    # Exactly one request reached the wire: the probe. The ceiling was spent by
    # it, so the agent loop sent nothing.
    assert len(endpoint.posts) == 1, endpoint.posts
    assert "turn_limit" in (result.stderr + result.stdout)


# --------------------------------------------------------------------------
# Opt-out regression (slice 01, default-off): without --capability-probe-payload
# the probe is off and behavior is unchanged. The run makes exactly one (loop)
# request and succeeds; a probe would add a first request before the loop.
# --------------------------------------------------------------------------

def test_probe_off_by_default_sends_no_probe_and_run_is_unchanged(daimonos_binary, tmp_path):
    endpoint = ProbeEndpoint("final_ok")
    try:
        workspace = tmp_path / "workspace"
        workspace.mkdir(exist_ok=True)
        state = tmp_path / "state"
        state.mkdir(mode=0o700, exist_ok=True)
        final_f = _write_final_output(tmp_path / "final.json")
        profile_f = _write_profile(tmp_path / "profile.json", endpoint.url)
        agent_env, env = _env(tmp_path)
        result = subprocess.run(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "agent", "--print", "--agent-env", str(agent_env),
             "--model-profile", str(profile_f), "--final-output", str(final_f),
             "review the evidence"],
            capture_output=True, text=True, timeout=TIMEOUT, env=env,
        )
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    # Exactly one request: the agent loop's. No probe was sent before it.
    assert len(endpoint.posts) == 1, endpoint.posts
    assert json.loads(result.stdout) == {"verdict": "supported"}, result.stdout


# ==========================================================================
# r2 integrator gaps G1-G5. See DIRECTION.md (run wp09-20261117-r2).
# ==========================================================================

def _run_controlled(daimonos_binary, tmp_path, *, endpoint_url, wire="messages",
                    request_retries=0, max_model_requests=None, corrective_retries=0,
                    audit_path=None, with_probe=False, payload=None):
    """A real controlled `agent --print` run with or without the capability
    probe. When ``with_probe`` is False no ``--capability-probe-payload`` is
    passed, so only the agent loop contacts the endpoint (used to exercise the
    final-output + audit terminal paths, G2/G3)."""
    workspace = tmp_path / "workspace"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    control = {
        "schema": FINAL_SCHEMA,
        "tool_name": FINAL_TOOL,
        "tool_description": "Emit the final verdict.",
        "corrective_retries": corrective_retries,
    }
    if max_model_requests is not None:
        control["max_model_requests"] = max_model_requests
    final_f = tmp_path / "final.json"
    final_f.write_text(json.dumps(control))
    profile_f = _write_profile(tmp_path / "profile.json", endpoint_url, wire,
                               request_retries)
    agent_env, env = _env(tmp_path)
    argv = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
    if audit_path is not None:
        argv += ["--audit-path", str(audit_path)]
    argv += ["agent", "--print", "--agent-env", str(agent_env),
             "--model-profile", str(profile_f), "--final-output", str(final_f)]
    if with_probe:
        payload_f = _write_payload(tmp_path / "payload.json", payload)
        argv += ["--capability-probe-payload", str(payload_f)]
    argv += ["review the evidence"]
    return subprocess.run(argv, capture_output=True, text=True, timeout=TIMEOUT,
                          env=env)


def _run_end(path):
    events = _read_events(path)
    ends = [e for e in events if e.get("kind") == "run_end"]
    assert len(ends) == 1, events
    return ends[0]


# --------------------------------------------------------------------------
# G1: a FAILED probe with --audit-path must record run_end exit_code 21 and a
# status naming `model_unsupported`, and the process must exit 21. WP08 made
# `run_terminal_status` the single source so the audit and the process agree;
# before the fix the probe's Aborted result mapped to ("aborted", 0) in run_end
# while the process still exited 21.
# --------------------------------------------------------------------------

def test_g1_failed_probe_run_end_exit_code_matches_process(daimonos_binary, tmp_path):
    endpoint = ProbeEndpoint("text_only")
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    end = _run_end(audit)
    assert end["exit_code"] == 21, end
    assert end["status"] == "model_unsupported", end


# --------------------------------------------------------------------------
# G2: with --final-output and --audit-path, a run that ends with exit 20
# (structured_output_failed) or exit 24 (model_request_limit) must write a
# run_end record carrying the SAME exit code (the single-source rule, G1's
# sibling for the final-output verdict). No probe is involved here.
# --------------------------------------------------------------------------

def test_g2_structured_output_failure_run_end_exit_20(daimonos_binary, tmp_path):
    # Loop gets text only (no final call) with no corrective retries => the
    # final-output decision is a terminal failure (exit 20).
    endpoint = ProbeEndpoint("text_only")
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_controlled(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                                 corrective_retries=0, audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 20, result.stderr + result.stdout
    end = _run_end(audit)
    assert end["exit_code"] == 20, end


def test_g2_model_request_limit_run_end_exit_24(daimonos_binary, tmp_path):
    # Loop gets text only; a corrective turn is permitted but the inclusive
    # model-request ceiling (1) is already spent by the first request, so the
    # run ends model_request_limit (exit 24) before a second request.
    endpoint = ProbeEndpoint("text_only")
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_controlled(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                                 corrective_retries=1, max_model_requests=1,
                                 audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 24, result.stderr + result.stdout
    assert len(endpoint.posts) == 1, endpoint.posts
    end = _run_end(audit)
    assert end["exit_code"] == 24, end


# --------------------------------------------------------------------------
# G3: the WP07 controlled transport feeds the audit. A loop model_request
# recorded for a real controlled run against a local fake endpoint must carry
# the transport's observed http_status and the profile's wire (ADR-026 model
# fields). Before the fix these were explicit-unavailable (null / absent).
# --------------------------------------------------------------------------

def test_g3_controlled_200_records_http_status_and_wire(daimonos_binary, tmp_path):
    endpoint = ProbeEndpoint("final_ok")
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_controlled(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                                 wire="messages", audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    events = _read_events(audit)
    reqs = [e for e in events if e.get("kind") == "model_request"]
    assert len(reqs) == 1, events
    assert reqs[0]["http_status"] == 200, reqs[0]
    assert reqs[0]["wire"] == "messages", reqs[0]
    assert reqs[0]["retry_index"] == 0, reqs[0]


# --------------------------------------------------------------------------
# G4: FR-04 reqs 18-19 and ADR-026 require the audit to record every request
# to the model endpoint. The probe is one such request. Rule applied (I-01
# amended): rather than emit a separate `model_request` (which would need a
# request_sequence slot coordinated with the loop that owns it), the probe's
# single request is recorded by ENRICHING the one `probe_result` event with
# the transport's http_status, wire, and retry_index (DIRECTION.md G4 permits
# either). So `probe_result` carries both the verdict and the request metadata.
# --------------------------------------------------------------------------

@pytest.mark.parametrize("mode,status", [("text_only", 200), ("503", 503)])
def test_g4_probe_result_records_request_metadata(daimonos_binary, tmp_path, mode, status):
    endpoint = ProbeEndpoint(mode)
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                            audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    events = _read_events(audit)
    probe = [e for e in events if e.get("kind") == "probe_result"]
    assert len(probe) == 1, events
    assert probe[0]["status"] == "model_unsupported", probe[0]
    assert probe[0]["http_status"] == status, probe[0]
    assert probe[0]["wire"] == "messages", probe[0]
    assert probe[0]["retry_index"] == 0, probe[0]
    assert CRED_VALUE not in audit.read_text()


# --------------------------------------------------------------------------
# G5: when the profile's credential variable is UNSET the probe is not built.
# The run must end with the SAME named startup error as a run without the probe
# (the controlled provider cannot resolve its credential), send no request, and
# must NOT silently run on without the probe. Credential value absent, so the
# error is identical whether or not --capability-probe-payload is present.
# --------------------------------------------------------------------------

def _run_controlled_no_cred(daimonos_binary, tmp_path, endpoint_url, *, with_probe):
    tmp_path.mkdir(parents=True, exist_ok=True)
    workspace = tmp_path / "workspace"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    final_f = _write_final_output(tmp_path / "final.json")
    profile_f = _write_profile(tmp_path / "profile.json", endpoint_url)
    agent_env, env = _env(tmp_path)
    env.pop(CRED_ENV, None)  # the profile's credential variable is unset
    argv = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
            "agent", "--print", "--agent-env", str(agent_env),
            "--model-profile", str(profile_f), "--final-output", str(final_f)]
    if with_probe:
        payload_f = _write_payload(tmp_path / "payload.json")
        argv += ["--capability-probe-payload", str(payload_f)]
    argv += ["review the evidence"]
    return subprocess.run(argv, capture_output=True, text=True, timeout=TIMEOUT, env=env)


def test_g5_unset_credential_same_error_with_or_without_probe(daimonos_binary, tmp_path):
    with_ep = _UnusedEndpoint()
    without_ep = _UnusedEndpoint()
    try:
        with_probe = _run_controlled_no_cred(daimonos_binary, tmp_path / "a",
                                             with_ep.url, with_probe=True)
        without_probe = _run_controlled_no_cred(daimonos_binary, tmp_path / "b",
                                                without_ep.url, with_probe=False)
    finally:
        with_ep.close()
        without_ep.close()
    # Same named startup error, same exit code, whether or not the probe flag is
    # present; no request is ever sent (the probe is not built, and the
    # controlled provider refuses before any model work).
    assert with_probe.returncode != 0, with_probe.stderr + with_probe.stdout
    assert with_probe.returncode == without_probe.returncode, \
        (with_probe.returncode, without_probe.returncode)
    assert CRED_ENV in (with_probe.stderr + with_probe.stdout)
    assert CRED_ENV in (without_probe.stderr + without_probe.stdout)
    assert with_ep.requests == [], with_ep.requests
    assert without_ep.requests == [], without_ep.requests


# ==========================================================================
# r3 reviewer findings R2-R7. See DIRECTION.md (run wp09-20261117-r3) and the
# reviewer report wp09-e36f108/review-wp09.md Findings section. These close the
# MINOR test gaps around the "exactly the same tool definitions" clause (R2),
# the "text/reasoning alongside the call is allowed" clause (R3), the
# tool-definition / HTTP-400 endpoint-rejection case (R4), and the "no
# byte-for-byte equality with the suggested payload" clause (R7).
# ==========================================================================


def test_r2_probe_tools_equal_first_loop_request_tools(daimonos_binary, tmp_path):
    # Req 15 / ADR-027: the probe offers EXACTLY the tool definitions the run
    # will offer. Both the probe and the first loop request reach the same fake
    # endpoint; assert their `tools` arrays are equal (names AND schemas), so a
    # regression that offered the model a different catalog in the probe than in
    # the run would be caught.
    endpoint = ProbeEndpoint("final_ok")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    posts = endpoint.posts
    assert len(posts) >= 2, posts  # [0] = probe, [1] = first loop request
    probe_tools = posts[0]["body"]["tools"]
    loop_tools = posts[1]["body"]["tools"]
    by_name = lambda t: t["name"]
    assert sorted(probe_tools, key=by_name) == sorted(loop_tools, key=by_name), \
        (probe_tools, loop_tools)


def test_r3_probe_text_block_alongside_final_call_passes(daimonos_binary, tmp_path):
    # Req 16 / ADR-027: a probe response carrying a text (reasoning) block
    # ALONGSIDE the single valid final-answer call is a pass (exit 0); the text
    # beside the sole final call is allowed.
    endpoint = ProbeEndpoint("text_plus_final")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout


def test_r4_probe_http_400_is_endpoint_error_and_one_request(daimonos_binary, tmp_path):
    # Req 17 / ADR-027: an endpoint error (including a tool-definition rejection
    # surfaced as HTTP 400) yields the distinct model_unsupported exit (21) with
    # reason `endpoint_error` and exactly one request (no retry).
    endpoint = ProbeEndpoint("400")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 21, result.stderr + result.stdout
    assert len(endpoint.posts) == 1, endpoint.posts
    blob = result.stderr + result.stdout
    assert "model_unsupported" in blob, blob
    assert "endpoint_error" in blob, blob


def test_r7_valid_but_different_payload_passes(daimonos_binary, tmp_path):
    # ADR-027: a probe response whose valid final call carries a verdict that
    # DIFFERS from the suggested payload yet still validates against the schema
    # is a pass (exit 0). The probe demands schema validity, not byte equality
    # with the suggested payload.
    endpoint = ProbeEndpoint("final_different")
    try:
        result = _run_probe(daimonos_binary, tmp_path, endpoint_url=endpoint.url)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout


# ==========================================================================
# r3 G6 (integrator, from the r2 diff). INTERFACES.md: "Record every attempted
# request." The WP07 controlled transport may retry inside ONE completion
# (`request_retries`), but r2's single `ControlledObservation` kept only the
# last attempt, so a retried controlled request produced a single
# `model_request` audit record. G6: the audit must record EVERY attempt.
# ==========================================================================


class SequenceEndpoint:
    """Fake Messages endpoint that answers a fixed SEQUENCE of HTTP statuses,
    one per request in order (repeating the last for any extra), so a single
    controlled completion that the transport retries can be observed
    attempt-by-attempt. A 200 carries the valid final-answer body; any other
    status carries an empty JSON body."""

    def __init__(self, statuses):
        self.statuses = list(statuses)
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0) or 0)
                if length:
                    self.rfile.read(length)
                idx = len(owner.requests)
                status = owner.statuses[min(idx, len(owner.statuses) - 1)]
                owner.requests.append(status)
                payload = json.dumps(_FINAL_OK).encode() if status == 200 else b"{}"
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

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


def test_g6_controlled_retries_record_every_attempt(daimonos_binary, tmp_path):
    # request_retries=2 against an endpoint answering 503, 503, 200: the one
    # controlled completion makes three HTTP attempts, so the audit must hold
    # THREE model_request records with retry_index 0,1,2 and the three statuses
    # 503,503,200 (not a single record of the last attempt).
    endpoint = SequenceEndpoint([503, 503, 200])
    audit = tmp_path / "state" / "audit.jsonl"
    try:
        result = _run_controlled(daimonos_binary, tmp_path, endpoint_url=endpoint.url,
                                 request_retries=2, audit_path=audit)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    assert endpoint.requests == [503, 503, 200], endpoint.requests
    events = _read_events(audit)
    reqs = [e for e in events if e.get("kind") == "model_request"]
    assert len(reqs) == 3, reqs
    assert [r["retry_index"] for r in reqs] == [0, 1, 2], reqs
    assert [r["http_status"] for r in reqs] == [503, 503, 200], reqs
    assert all(r["wire"] == "messages" for r in reqs), reqs
