"""WP07 (FR-04 transport, requirements 1-13) acceptance tests.

Context: docs/adr/025-controlled-model-transport.md; the reviewer-runtime
INTERFACES ledger row for WP07. These exercise the opt-in controlled model
transport through the real `daimonos` CLI, without FR-01 (the capability probe,
WP09) or the audit record (WP08).

Scope here is what is observable through the process WITHOUT a model request:
the `--model-profile` startup validation (AC-04.7 and the no-mixing rule), and
the default-off opt-out regression. The exact body/header/retry/redirect
behavior of the transport itself (AC-04.1-04.5) is asserted at the transport
level by `cargo test providers::controlled_transport_tests`; the integrated
probe repeats them end to end in WP09.

No live credentialed endpoint is used anywhere.
"""

import json
import subprocess

import pytest

# Dry-run never calls a provider, loads no agent env, and opens no socket, so
# these complete fast; a generous ceiling still guards against a hang.
TIMEOUT = 60


def _run_agent(daimonos_binary, workspace, *extra, task="inspect the workspace"):
    return subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "agent", "--dry-run", *extra, task],
        capture_output=True,
        text=True,
        timeout=TIMEOUT,
    )


def _write_profile(path, **overrides):
    profile = {
        "provider_label": "anthropic",
        "model": "claude-opus-5-5",
        "base_url": "http://127.0.0.1:1",
        "wire": "messages",
        "credential_env": "DBUNK_CTRL_KEY",
    }
    profile.update(overrides)
    # Allow deletion of required keys to build partial profiles.
    for key in [k for k, v in list(profile.items()) if v is _MISSING]:
        del profile[key]
    path.write_text(json.dumps(profile))
    return path


class _Missing:
    pass


_MISSING = _Missing()


def test_agent_help_lists_model_profile(daimonos_binary):
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True,
        text=True,
        timeout=5,
        check=True,
    )
    assert "--model-profile" in completed.stdout


def test_partial_profile_fails_at_startup_naming_missing_values(daimonos_binary, tmp_path):
    # AC-04.7: supplying only three of the five values fails at startup, naming
    # every missing value, before any request.
    profile = _write_profile(
        tmp_path / "partial.json",
        base_url=_MISSING,
        credential_env=_MISSING,
    )
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode != 0, result.stdout
    combined = result.stderr + result.stdout
    assert "missing" in combined, combined
    assert "base_url" in combined, combined
    assert "credential_env" in combined, combined


def test_single_missing_value_is_named(daimonos_binary, tmp_path):
    # AC-04.7, the four-of-five case: exactly the missing value is named.
    profile = _write_profile(tmp_path / "four.json", wire=_MISSING)
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode != 0
    combined = result.stderr + result.stdout
    assert "missing" in combined and "wire" in combined, combined
    assert "base_url" not in combined, combined


def test_wire_is_not_inferred_and_invalid_wire_is_refused(daimonos_binary, tmp_path):
    # FR-04 req 3: the wire is never inferred; an invalid value is refused by name.
    profile = _write_profile(tmp_path / "badwire.json", wire="guess")
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode != 0
    assert "wire" in (result.stderr + result.stdout)


def test_unconfigured_extra_field_is_refused(daimonos_binary, tmp_path):
    # FR-04 req 6 default: a field beyond the known set fails unless explicitly
    # configured (sampling is never an explicit controlled field).
    path = tmp_path / "extra.json"
    data = json.loads(_write_profile(tmp_path / "base.json").read_text())
    data["temperature"] = 0.7
    path.write_text(json.dumps(data))
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(path))
    assert result.returncode != 0
    assert "unconfigured" in (result.stderr + result.stdout)


def test_profile_cannot_be_combined_with_model_or_provider_flags(daimonos_binary, tmp_path):
    # FR-04 req 2: values must not be combined from different sources. The flag
    # is mutually exclusive with --model/--provider at the CLI boundary.
    profile = _write_profile(tmp_path / "full.json")
    result = _run_agent(
        daimonos_binary, tmp_path, "--model-profile", str(profile), "--model", "other"
    )
    assert result.returncode != 0
    assert "model-profile" in (result.stderr + result.stdout)


def test_complete_profile_is_accepted(daimonos_binary, tmp_path):
    # A complete profile validates at startup; dry-run then proceeds with no
    # request, proving the five values plus request settings parse.
    profile = _write_profile(
        tmp_path / "full.json",
        wire="chat",
        provider_label="openrouter",
        model="some/model",
        base_url="http://127.0.0.1:1",
        max_tokens=4096,
        timeout_ms=30000,
        request_retries=0,
    )
    result = _run_agent(daimonos_binary, tmp_path, "--model-profile", str(profile))
    assert result.returncode == 0, result.stderr + result.stdout


def test_default_off_is_the_legacy_dry_run(daimonos_binary, tmp_path):
    # Opt-out regression: with no --model-profile the agent behaves exactly as
    # before (a plain dry-run succeeds and prints the tool/task preview).
    result = _run_agent(daimonos_binary, tmp_path)
    assert result.returncode == 0, result.stderr + result.stdout


# ---------------------------------------------------------------------------
# Process-level transport proof (integrator review H3/H4).
#
# These start a local fake model endpoint, run the REAL `daimonos agent
# --print --model-profile PROFILE` process, and assert on the HTTP request the
# endpoint records. They prove the controlled transport is wired into the live
# send-path (not only reachable from a unit test), for both wires where the
# behavior differs. No live credentialed endpoint is ever contacted; every
# request stays on 127.0.0.1. The fake answer is plain text so the agent turn
# ends after exactly one model request.
#
# Fake-endpoint pattern follows tests/test_state_frontends.py and
# tests/test_path_confinement.py.
# ---------------------------------------------------------------------------

import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# The credential env var every controlled profile below names. Its VALUE must
# never reach the recorded request except inside the single allowed auth header.
CRED_ENV = "DBUNK_CTRL_KEY"
CRED_VALUE = "sk-ctrl-PROCESS-SECRET-1a2b3c"

# Wire-appropriate plain-text answers. The controlled path is non-streaming, so
# the chat answer is a normal (non-SSE) Chat Completions object.
_MESSAGES_OK = json.dumps(
    {"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
     "usage": {"input_tokens": 1, "output_tokens": 1}}
)
_CHAT_OK = json.dumps(
    {"choices": [{"message": {"role": "assistant", "content": "done"},
                  "finish_reason": "stop"}],
     "usage": {"prompt_tokens": 1, "completion_tokens": 1}}
)


class RecordingEndpoint:
    """A local fake model endpoint that records every HTTP request it accepts.

    ``mode`` selects the reply: ``ok`` (200 + a wire-appropriate plain-text
    answer), ``503`` (always 503), or ``307`` (a redirect whose Location points
    at ``redirect_to``). ``requests`` collects ``{method, path, headers, body}``
    for every accepted connection.
    """

    def __init__(self, wire, mode="ok", redirect_to=None):
        self.wire = wire
        self.mode = mode
        self.redirect_to = redirect_to
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def _answer_body(self):
                return _MESSAGES_OK if owner.wire == "messages" else _CHAT_OK

            def _record(self, method):
                length = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = raw.decode("utf-8", "replace")
                owner.requests.append({
                    "method": method,
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body": body,
                })

            def _reply(self):
                if owner.mode == "503":
                    payload = b"{}"
                    self.send_response(503)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                if owner.mode == "307":
                    payload = b"{}"
                    self.send_response(307)
                    self.send_header("Location", owner.redirect_to or "http://127.0.0.1:1/x")
                    self.send_header("Content-Length", str(len(payload)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                payload = self._answer_body().encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                self._record("POST")
                self._reply()

            def do_GET(self):
                # Controlled mode performs no model discovery; a GET here would
                # be a hidden catalog probe. Record it so tests can assert none.
                self._record("GET")
                payload = b'{"data": []}'
                self.send_response(200)
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

    @property
    def post_requests(self):
        return [r for r in self.requests if r["method"] == "POST"]

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self._thread.join(timeout=3)


def _print_env(tmp_path, agent_base, extra_env=None):
    """Build a clean subprocess environment for a real `agent --print` run.

    Controlled mode performs no model discovery, so the context window is set
    explicitly in the agent env. The agent-env base URL deliberately points at
    an UNUSED port: when a controlled profile is set it is the SOLE source of
    base_url, so the request must still reach the profile's endpoint.
    """
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
        CRED_ENV_SENTINEL="1",
    )
    env[CRED_ENV] = CRED_VALUE
    if extra_env:
        env.update(extra_env)
    return agent_env, env


def _run_print(daimonos_binary, tmp_path, endpoint, wire, extra_env=None,
               provider_label="anthropic", model="claude-opus-5-5",
               request_retries=0):
    workspace = tmp_path / "workspace"
    workspace.mkdir(exist_ok=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700, exist_ok=True)
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({
        "provider_label": provider_label,
        "model": model,
        "base_url": endpoint.url,
        "wire": wire,
        "credential_env": CRED_ENV,
        "request_retries": request_retries,
        "max_tokens": 256,
    }))
    agent_env, env = _print_env(tmp_path, "http://127.0.0.1:1", extra_env)
    return subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "agent", "--print", "--agent-env", str(agent_env),
         "--model-profile", str(profile), "say hello"],
        capture_output=True, text=True, timeout=TIMEOUT, env=env,
    )


def test_process_messages_body_is_the_fixed_set_no_sampling(daimonos_binary, tmp_path):
    # H3(a) / AC-04.1, Messages wire: the live process sends exactly the fixed
    # body fields and no sampling field.
    endpoint = RecordingEndpoint("messages")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "messages")
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    posts = endpoint.post_requests
    assert len(posts) == 1, posts
    assert posts[0]["path"] == "/v1/messages", posts[0]["path"]
    body = posts[0]["body"]
    assert set(body) == {"model", "max_tokens", "system", "messages", "tools", "tool_choice"}, body
    for sampling in ("temperature", "top_p", "top_k", "stream"):
        assert sampling not in body, f"sampling field {sampling} present"
    assert endpoint.requests[0]["method"] == "POST", "no hidden GET discovery"


def test_process_chat_body_is_the_fixed_set_no_sampling(daimonos_binary, tmp_path):
    # H3(a) / AC-04.1, Chat wire.
    endpoint = RecordingEndpoint("chat")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "chat",
                            provider_label="openrouter", model="some/model")
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    posts = endpoint.post_requests
    assert len(posts) == 1, posts
    assert posts[0]["path"] == "/chat/completions", posts[0]["path"]
    body = posts[0]["body"]
    assert set(body) == {"model", "messages", "tools", "tool_choice"}, body
    for sampling in ("temperature", "top_p", "top_k", "stream"):
        assert sampling not in body, f"sampling field {sampling} present"


_HOSTILE_ENV = {
    "OPENAI_ORG_ID": "org-HOSTILE",
    "OPENAI_PROJECT_ID": "proj-HOSTILE",
    "OPENAI_CUSTOM_HEADERS": "x-leak: 1",
    "ANTHROPIC_CUSTOM_HEADERS": "x-leak: 1",
    "HTTP_PROXY": "http://127.0.0.1:1",
    "HTTPS_PROXY": "http://127.0.0.1:1",
    "ALL_PROXY": "http://127.0.0.1:1",
}
_TRANSPORT_HEADERS = {"host", "content-length", "accept", "accept-encoding",
                      "connection", "user-agent"}
_LEAK_HEADERS = ("openai-organization", "openai-project", "x-organization",
                 "x-leak", "traceparent")


def test_process_messages_headers_are_fixed_despite_hostile_env(daimonos_binary, tmp_path):
    # H3(b) / AC-04.2, Messages wire: org/project/custom-header and proxy env
    # vars are set, yet the request reaches the profile endpoint directly (no
    # proxy) with only the fixed header set.
    endpoint = RecordingEndpoint("messages")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "messages",
                            extra_env=_HOSTILE_ENV)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    posts = endpoint.post_requests
    assert len(posts) == 1, "proxy env must be ignored; request reaches endpoint directly"
    headers = posts[0]["headers"]
    assert headers["x-api-key"] == CRED_VALUE
    assert headers["anthropic-version"] == "2023-06-01"
    assert headers["content-type"] == "application/json"
    assert headers["user-agent"].startswith("daimonos/")
    app_headers = {k for k in headers if k not in _TRANSPORT_HEADERS}
    assert app_headers == {"x-api-key", "anthropic-version", "content-type"}, app_headers
    for leaked in _LEAK_HEADERS:
        assert leaked not in headers, f"leaked header {leaked}"


def test_process_chat_headers_are_fixed_despite_hostile_env(daimonos_binary, tmp_path):
    # H3(b) / AC-04.2, Chat wire.
    endpoint = RecordingEndpoint("chat")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "chat",
                            provider_label="openrouter", model="some/model",
                            extra_env=_HOSTILE_ENV)
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    posts = endpoint.post_requests
    assert len(posts) == 1, "proxy env must be ignored; request reaches endpoint directly"
    headers = posts[0]["headers"]
    assert headers["authorization"] == f"Bearer {CRED_VALUE}"
    assert "x-api-key" not in headers
    app_headers = {k for k in headers if k not in _TRANSPORT_HEADERS}
    assert app_headers == {"authorization", "content-type"}, app_headers
    for leaked in _LEAK_HEADERS:
        assert leaked not in headers, f"leaked header {leaked}"


def test_process_503_produces_exactly_one_request(daimonos_binary, tmp_path):
    # H3(c) / AC-04.3: with request_retries=0 a 503 yields exactly one request.
    # The controlled transport owns the attempt count; the agent loop must add
    # no hidden retry of its own.
    endpoint = RecordingEndpoint("messages", mode="503")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "messages",
                            request_retries=0)
    finally:
        endpoint.close()
    assert result.returncode != 0, result.stdout
    posts = endpoint.post_requests
    assert len(posts) == 1, f"expected exactly one request, got {len(posts)}"


def test_process_307_not_followed_and_second_host_untouched(daimonos_binary, tmp_path):
    # H3(d) / AC-04.4: a 307 is a terminal endpoint error; it is not followed,
    # the body is never re-sent, and a second local listener receives nothing.
    second = RecordingEndpoint("messages")
    first = RecordingEndpoint("messages", mode="307",
                             redirect_to=second.url + "/v1/messages")
    try:
        result = _run_print(daimonos_binary, tmp_path, first, "messages")
    finally:
        first.close()
        second.close()
    assert result.returncode != 0, result.stdout
    assert len(first.post_requests) == 1, "exactly one request to the first host"
    assert len(second.requests) == 0, "the redirect target must receive nothing"


def test_process_messages_tool_choice_is_never_forced(daimonos_binary, tmp_path):
    # H3(e) / AC-04.5, Messages wire: automatic, never forced.
    endpoint = RecordingEndpoint("messages")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "messages")
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    body = endpoint.post_requests[0]["body"]
    assert body["tool_choice"] == {"type": "auto"}, body["tool_choice"]
    assert body["tool_choice"].get("type") not in ("any", "required", "tool")


def test_process_chat_tool_choice_is_never_forced(daimonos_binary, tmp_path):
    # H3(e) / AC-04.5, Chat wire.
    endpoint = RecordingEndpoint("chat")
    try:
        result = _run_print(daimonos_binary, tmp_path, endpoint, "chat",
                            provider_label="openrouter", model="some/model")
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    body = endpoint.post_requests[0]["body"]
    assert body["tool_choice"] == "auto", body["tool_choice"]
    assert body["tool_choice"] not in ("required", "any", "none")


# ---------------------------------------------------------------------------
# H4: opt-out regression at the process level.
#
# Without --model-profile the legacy provider path is used unchanged. The
# controlled-mode constraints (no sampling, no provider `strict`, forced
# non-streaming, fixed tool_choice) do NOT apply. Verified here by recording
# the legacy request and asserting it still carries the legacy `strict: false`
# tool flag and streams — behavior WP07's additive branch leaves untouched.
# (WP07's only agent_runtime change is an additive `if controlled_profile {..}`
# branch; the `else` legacy branch is byte-identical to master.)
# ---------------------------------------------------------------------------

class _LegacySseEndpoint:
    """Legacy OpenRouter-compatible endpoint: SSE answer + a models catalog."""

    def __init__(self):
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{"id": "test/model",
                    "context_length": 100000, "supported_parameters": ["tools"],
                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(length) if length else b""
                owner.requests.append(json.loads(raw) if raw else None)
                chunk = {"choices": [{"index": 0,
                        "delta": {"role": "assistant", "content": "done"},
                        "finish_reason": "stop"}]}
                payload = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
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


def test_process_default_off_keeps_legacy_request_unchanged(daimonos_binary, tmp_path):
    # H4: no --model-profile -> legacy path unchanged (controlled constraints
    # are not applied). The legacy tool flag `strict: false` (#1526), which the
    # controlled wire removes, must still be present.
    endpoint = _LegacySseEndpoint()
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    agent_env, env = _print_env(tmp_path, endpoint.url)
    # Legacy run: provider/model from the agent env, NO --model-profile.
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        f"DAIMONOS_AGENT_BASE_URL={endpoint.url}",
        "DAIMONOS_AGENT_API_KEY=legacy-key",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        "DAIMONOS_AGENT_CONTEXT_WINDOW=100000",
        "",
    ]))
    try:
        result = subprocess.run(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "agent", "--print", "--agent-env", str(agent_env), "say hello"],
            capture_output=True, text=True, timeout=TIMEOUT, env=env,
        )
    finally:
        endpoint.close()
    assert result.returncode == 0, result.stderr + result.stdout
    assert endpoint.requests, "legacy path must have sent a request"
    body = endpoint.requests[-1]
    # Legacy behavior WP07 does not touch: tools pin strict:false, and the
    # request streams. Neither is true of the controlled wire.
    assert body.get("stream") is True, body
    assert body["tools"][0]["function"]["strict"] is False, body["tools"][0]


# ---------------------------------------------------------------------------
# R1 (review r3): fail-closed frontend check.
#
# `--model-profile` is honored only by the one-shot print send-path. The
# interactive TUI returns through the session daemon, which has no profile
# field, so an interactive run could only use the legacy transport. A
# security-critical opt-in must never fall open to legacy: the flag is mutually
# exclusive with --interactive at the CLI boundary, so the combination is
# refused at startup (parse time, before any request or TTY check). Interactive
# mode is reachable ONLY via the --interactive flag (tui::resolve_agent_mode),
# so this closes every path to it. Chat/ACP subcommands expose no
# --model-profile flag at all, so they cannot be handed a profile.
# ---------------------------------------------------------------------------


def test_interactive_with_model_profile_is_refused_fail_closed(daimonos_binary, tmp_path):
    profile = _write_profile(tmp_path / "full.json")
    result = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path), "agent",
         "--interactive", "--model-profile", str(profile), "a task"],
        capture_output=True, text=True, timeout=TIMEOUT,
    )
    assert result.returncode != 0, result.stdout
    combined = result.stderr + result.stdout
    # A named, fail-closed refusal of the combination — not a silent legacy run.
    assert "--model-profile" in combined, combined
    assert "--interactive" in combined, combined
    assert "cannot be used with" in combined, combined


def test_chat_and_acp_expose_no_model_profile_flag(daimonos_binary):
    # The other frontends cannot be handed a profile at all: the flag is absent,
    # so there is no silent-legacy path to close for them.
    for sub in ("chat", "acp"):
        completed = subprocess.run(
            [daimonos_binary, sub, "--help"],
            capture_output=True, text=True, timeout=5, check=True,
        )
        assert "--model-profile" not in completed.stdout, sub


# ---------------------------------------------------------------------------
# R5 (review r3): ambient proxy is ignored, proven with a COUNTING proxy.
#
# HTTP_PROXY / HTTPS_PROXY / ALL_PROXY are pointed at a local server that counts
# every connection. The controlled client calls reqwest `.no_proxy()`, so the
# request must reach the profile endpoint DIRECTLY and the proxy must receive
# nothing. (An EXPLICIT `proxy` value being honored is proven at the transport
# level in providers::controlled_transport_tests.)
# ---------------------------------------------------------------------------


class _CountingProxy:
    """A local server counting every connection it receives (POST/GET/CONNECT)."""

    def __init__(self):
        self.hits = 0
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def _count_and_reply(self):
                owner.hits += 1
                payload = b"{}"
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0) or 0)
                if length:
                    self.rfile.read(length)
                self._count_and_reply()

            def do_GET(self):
                self._count_and_reply()

            def do_CONNECT(self):
                owner.hits += 1
                self.send_response(200)
                self.end_headers()

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


def test_process_ambient_proxy_is_ignored_counting_proxy_untouched(daimonos_binary, tmp_path):
    endpoint = RecordingEndpoint("messages")
    proxy = _CountingProxy()
    try:
        result = _run_print(
            daimonos_binary, tmp_path, endpoint, "messages",
            extra_env={"HTTP_PROXY": proxy.url, "HTTPS_PROXY": proxy.url,
                       "ALL_PROXY": proxy.url},
        )
    finally:
        endpoint.close()
        proxy.close()
    assert result.returncode == 0, result.stderr + result.stdout
    assert len(endpoint.post_requests) == 1, "request must reach the endpoint directly"
    assert proxy.hits == 0, f"ambient proxy must receive nothing, got {proxy.hits}"
