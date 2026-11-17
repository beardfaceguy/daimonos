"""WP05 structured final output — observable CLI behavior (ADR-024, FR-01).

Context: docs/adr/024-validated-terminal-output.md,
docs/contexts/reviewer-runtime/INTERFACES.md.

These are integration tests: they drive the real `daimonos agent --final-output`
process against a local scripted model endpoint (no live API, no real
credentials). Each FR-01 acceptance criterion is exercised on BOTH mock wires —
Chat Completions (`openrouter`) and Messages (`anthropic`) — because ADR-024's
confirmation requires both. The Rust foundation (schema_validation.rs,
structured_output.rs) is additionally covered by in-crate unit tests.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest


# --- CLI presence: a missing flag is a hard failure, never a skip -----------


def _agent_help(daimonos_binary) -> str:
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    return (completed.stdout or "") + (completed.stderr or "")


@pytest.fixture(scope="module", autouse=True)
def _require_final_output(daimonos_binary):
    assert "--final-output" in _agent_help(daimonos_binary), (
        "`agent --final-output` must be wired; a missing flag fails this suite."
    )


def test_agent_help_lists_final_output_flag(daimonos_binary):
    assert "--final-output" in _agent_help(daimonos_binary)


# --- Shared schema + control-file helper ------------------------------------

SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "answer": {"type": "string"},
        "when": {"type": "string", "format": "date-time"},
    },
    "required": ["answer"],
    "additionalProperties": False,
}


def _write_control(tmp_path, **fields) -> str:
    path = tmp_path / "final_output.json"
    path.write_text(json.dumps(fields))
    return str(path)


# --- Abstract scripted turns, rendered per wire -----------------------------
#
# A "turn" is {"text": str|None, "calls": [{"id","name","args"}]}. Each mock
# translates it to that wire's SSE. One POST consumes one turn; the last turn
# repeats so a never-ending model still terminates the loop deterministically.


def turn(calls=None, text=None, stop=None):
    # `stop` overrides the wire stop/finish reason (e.g. "max_tokens" to force a
    # truncated turn); default is tool_calls when calls are present, else stop.
    return {"text": text, "calls": list(calls or []), "stop": stop}


def final_call(args, call_id="final-1"):
    return {"id": call_id, "name": "final_answer", "args": args}


def _isolated_env(home, **extra):
    """A hermetic subprocess environment: an isolated HOME + XDG so no file of
    the host user is read, and no inherited DAIMONOS_/XDG_ settings leak in."""
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("DAIMONOS_", "XDG_"))}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"))
    env.update(extra)
    return env


def _chat_unanswered_calls(body):
    """True when the request's last assistant `tool_calls` are not all answered
    by a following `tool` message (OpenAI/OpenRouter Chat Completions wire)."""
    msgs = body.get("messages", [])
    last = None
    for i, m in enumerate(msgs):
        if m.get("role") == "assistant" and m.get("tool_calls"):
            last = i
    if last is None:
        return False
    needed = {tc.get("id") for tc in msgs[last].get("tool_calls", [])}
    answered = {m.get("tool_call_id") for m in msgs[last + 1:]
                if m.get("role") == "tool" and m.get("tool_call_id") is not None}
    return not needed.issubset(answered)


def _messages_unanswered_calls(body):
    """True when the request's last assistant `tool_use` blocks are not all
    answered by a following `tool_result` block (Anthropic Messages wire)."""
    msgs = body.get("messages", [])

    def tool_use_ids(content):
        if not isinstance(content, list):
            return []
        return [b.get("id") for b in content
                if isinstance(b, dict) and b.get("type") == "tool_use"]

    last = None
    for i, m in enumerate(msgs):
        if m.get("role") == "assistant" and tool_use_ids(m.get("content")):
            last = i
    if last is None:
        return False
    needed = set(tool_use_ids(msgs[last].get("content")))
    answered = set()
    for m in msgs[last + 1:]:
        content = m.get("content")
        if isinstance(content, list):
            for b in content:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    answered.add(b.get("tool_use_id"))
    return not needed.issubset(answered)


def _reject_unanswered(handler, raw, checker):
    """ADR-024 strict confirmation: a real endpoint rejects a request whose
    previous assistant tool call(s) were not all answered. Return True (and send
    HTTP 400) when `checker` flags the request, so the corrective-turn pairing
    is genuinely exercised rather than silently accepted."""
    try:
        body = json.loads(raw or b"{}")
    except (ValueError, TypeError):
        return False
    if not checker(body):
        return False
    payload = json.dumps({"error": {"message": "unanswered tool call"}}).encode()
    handler.send_response(400)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(payload)))
    handler.send_header("Connection", "close")
    handler.end_headers()
    handler.close_connection = True
    try:
        handler.wfile.write(payload)
        handler.wfile.flush()
    except (BrokenPipeError, ConnectionResetError):
        pass
    return True


class _MockChat:
    """Local scripted OpenAI/OpenRouter Chat Completions endpoint (SSE)."""

    def __init__(self, turns):
        self._turns = list(turns)
        self._index = 0
        self.post_count = 0
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):  # /models capability discovery
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
                raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                owner.post_count += 1
                if _reject_unanswered(self, raw, _chat_unanswered_calls):
                    return
                index = min(owner._index, len(owner._turns) - 1)
                owner._index += 1
                t = owner._turns[index]
                delta = {"role": "assistant"}
                if t["text"] is not None:
                    delta["content"] = t["text"]
                if t["calls"]:
                    delta["tool_calls"] = [{
                        "index": i, "id": c["id"], "type": "function",
                        "function": {"name": c["name"],
                                     "arguments": json.dumps(c["args"])},
                    } for i, c in enumerate(t["calls"])]
                finish = t.get("stop") or ("tool_calls" if t["calls"] else "stop")
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


class _MockMessages:
    """Local scripted Anthropic Messages endpoint (SSE)."""

    def __init__(self, turns):
        self._turns = list(turns)
        self._index = 0
        self.post_count = 0
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):  # /v1/models/{model} context-window probe
                payload = json.dumps({"id": "test/model", "type": "model",
                                      "display_name": "Test"}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def _event(self, name, data):
                return ("event: " + name + "\ndata: " + json.dumps(data) + "\n\n").encode()

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                owner.post_count += 1
                if _reject_unanswered(self, raw, _messages_unanswered_calls):
                    return
                index = min(owner._index, len(owner._turns) - 1)
                owner._index += 1
                t = owner._turns[index]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                blocks = []
                idx = 0
                if t["text"] is not None:
                    blocks.append(self._event("content_block_start", {
                        "type": "content_block_start", "index": idx,
                        "content_block": {"type": "text", "text": ""}}))
                    blocks.append(self._event("content_block_delta", {
                        "type": "content_block_delta", "index": idx,
                        "delta": {"type": "text_delta", "text": t["text"]}}))
                    idx += 1
                for c in t["calls"]:
                    blocks.append(self._event("content_block_start", {
                        "type": "content_block_start", "index": idx,
                        "content_block": {"type": "tool_use", "id": c["id"],
                                          "name": c["name"], "input": {}}}))
                    blocks.append(self._event("content_block_delta", {
                        "type": "content_block_delta", "index": idx,
                        "delta": {"type": "input_json_delta",
                                  "partial_json": json.dumps(c["args"])}}))
                    idx += 1
                stop = t.get("stop") or ("tool_use" if t["calls"] else "end_turn")
                blocks.append(self._event("message_delta", {
                    "type": "message_delta", "delta": {"stop_reason": stop},
                    "usage": {"output_tokens": 5}}))
                blocks.append(self._event("message_stop", {"type": "message_stop"}))
                try:
                    for b in blocks:
                        self.wfile.write(b)
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


WIRES = {
    "chat": ("openrouter", _MockChat),
    "messages": ("anthropic", _MockMessages),
}


def _run(binary, tmp_path, wire, turns, *, schema=None, corrective_retries=0,
         max_model_requests=None, tool_name="final_answer", debug=False,
         extra_files=None, final_output=True):
    provider, mock_cls = WIRES[wire]
    mock = mock_cls(turns)
    try:
        home = tmp_path / "home"
        home.mkdir(exist_ok=True)
        for name, body in (extra_files or {}).items():
            (tmp_path / name).write_text(body)
        config = tmp_path / "config.toml"
        config.write_text("[analytics]\nenabled=false\n[acp.mcp]\nenabled=false\n")
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            f"DAIMONOS_AGENT_PROVIDER={provider}",
            "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{mock.port}",
            "DAIMONOS_AGENT_API_KEY=synthetic-not-a-real-key",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_COMPACTION=off", ""]))
        fields = {"schema": schema if schema is not None else SCHEMA,
                  "tool_name": tool_name,
                  "tool_description": "Emit the final structured result.",
                  "corrective_retries": corrective_retries}
        if max_model_requests is not None:
            fields["max_model_requests"] = max_model_requests
        control = _write_control(tmp_path, **fields)
        argv = [binary, "--workspace", str(tmp_path), "--config", str(config),
                "agent"]
        if final_output:
            argv += ["--final-output", control]
        argv += ["--agent-env", str(agent_env),
                 "--print", "produce the final answer"]
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("DAIMONOS_", "XDG_"))}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"))
        if debug:
            env["RUST_LOG"] = "debug"
        completed = subprocess.run(
            argv, capture_output=True, text=True, timeout=120, env=env)
        return completed, mock.post_count
    finally:
        mock.close()


def _last_json(text):
    return json.loads(text.strip().splitlines()[-1])


# --- Model-driven acceptance criteria, exercised on BOTH wires --------------

BOTH_WIRES = pytest.mark.parametrize("wire", ["chat", "messages"])

COND_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "kind": {"type": "string"},
        "tags": {"type": "array", "uniqueItems": True},
        "id": {"$ref": "#/$defs/id"},
    },
    "required": ["kind", "id"],
    "if": {"properties": {"kind": {"const": "tagged"}}},
    "then": {"required": ["tags"]},
    "$defs": {"id": {"type": "string", "minLength": 4}},
}


@BOTH_WIRES
def test_ac_01_1_valid_answer_exact_stdout_bytes(daimonos_binary, tmp_path, wire):
    completed, posts = _run(
        daimonos_binary, tmp_path, wire, [turn([final_call({"answer": "done"})])])
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"done"}\n'
    assert posts == 1


@BOTH_WIRES
def test_ac_01_1_text_alongside_valid_call_still_succeeds(daimonos_binary, tmp_path, wire):
    completed, _ = _run(
        daimonos_binary, tmp_path, wire,
        [turn([final_call({"answer": "ok"})], text="Here is my conclusion.")])
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"ok"}\n'


@BOTH_WIRES
def test_ac_01_2_text_only_then_valid_sends_one_corrective(daimonos_binary, tmp_path, wire):
    completed, posts = _run(
        daimonos_binary, tmp_path, wire,
        [turn(text="I am thinking."), turn([final_call({"answer": "done"})])],
        corrective_retries=1)
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"done"}\n'
    # Exactly one corrective message was sent: a second request happened.
    assert posts == 2


@BOTH_WIRES
def test_ac_01_3_invalid_twice_exhausts_retries(daimonos_binary, tmp_path, wire):
    invalid = turn([final_call({"answer": 42})])  # answer must be a string
    completed, posts = _run(
        daimonos_binary, tmp_path, wire, [invalid, invalid], corrective_retries=1)
    assert completed.returncode == 20, completed.stderr
    assert completed.stdout == ""
    payload = _last_json(completed.stderr)
    assert payload["reason"] == "retries_exhausted"
    assert payload["attempts"] == 2
    assert payload["model_requests"] == 2
    assert payload["validation_errors"], "last validation errors must be preserved"
    assert posts == 2


@BOTH_WIRES
def test_ac_01_4_mixed_ending_executes_none_reports_mixed(daimonos_binary, tmp_path, wire):
    mixed = turn([
        {"id": "ev-1", "name": "read_file", "args": {"path": "evidence.txt"}},
        final_call({"answer": "done"}, call_id="fin-1"),
    ])
    completed, _ = _run(daimonos_binary, tmp_path, wire, [mixed],
                        corrective_retries=0,
                        extra_files={"evidence.txt": "x"})
    assert completed.returncode == 20, completed.stderr
    assert completed.stdout == ""
    assert _last_json(completed.stderr)["reason"] == "mixed_calls"


@BOTH_WIRES
def test_ac_01_4_mixed_corrective_answers_both_calls_then_succeeds(daimonos_binary, tmp_path, wire):
    # The corrective message carries a result for BOTH calls, so the follow-up
    # request is accepted (never rejected for an unanswered call) and succeeds.
    mixed = turn([
        {"id": "ev-1", "name": "read_file", "args": {"path": "evidence.txt"}},
        final_call({"answer": "done"}, call_id="fin-1"),
    ])
    completed, posts = _run(
        daimonos_binary, tmp_path, wire,
        [mixed, turn([final_call({"answer": "final"})])],
        corrective_retries=1, extra_files={"evidence.txt": "x"})
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"final"}\n'
    assert posts == 2


@BOTH_WIRES
def test_ac_01_5_conditional_rule_rejects(daimonos_binary, tmp_path, wire):
    # uniqueItems violation under the if/then + local-$ref schema.
    bad = turn([final_call({"kind": "tagged", "tags": ["a", "a"], "id": "abcd"})])
    completed, _ = _run(daimonos_binary, tmp_path, wire, [bad],
                        schema=COND_SCHEMA, corrective_retries=0)
    assert completed.returncode == 20, completed.stderr
    assert _last_json(completed.stderr)["reason"] == "invalid_arguments"


@BOTH_WIRES
def test_ac_01_7_debug_logging_keeps_stdout_clean(daimonos_binary, tmp_path, wire):
    completed, _ = _run(
        daimonos_binary, tmp_path, wire,
        [turn([final_call({"answer": "done"})])], debug=True)
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"done"}\n'


@BOTH_WIRES
def test_zero_retries_reports_actual_reason(daimonos_binary, tmp_path, wire):
    completed, posts = _run(
        daimonos_binary, tmp_path, wire, [turn([final_call({"answer": 42})])],
        corrective_retries=0)
    assert completed.returncode == 20, completed.stderr
    payload = _last_json(completed.stderr)
    assert payload["reason"] == "invalid_arguments"
    assert payload["attempts"] == 1
    assert posts == 1


@BOTH_WIRES
def test_evidence_turn_then_final_read_succeeds(daimonos_binary, tmp_path, wire):
    # An ordinary evidence tool turn continues the loop (consuming a model
    # request, not a corrective retry); the next turn's final answer succeeds.
    turns = [
        turn([{"id": "r1", "name": "read_file", "args": {"path": "evidence.txt"}}]),
        turn([final_call({"answer": "done"})]),
    ]
    completed, posts = _run(daimonos_binary, tmp_path, wire, turns,
                            corrective_retries=0,
                            extra_files={"evidence.txt": "hello"})
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"done"}\n'
    assert posts == 2


@BOTH_WIRES
def test_turn_limit_fails_with_turn_limit_reason(daimonos_binary, tmp_path, wire):
    # The inclusive model-request ceiling (1) is reached before a valid ending;
    # the run fails with exit 24 and reason `turn_limit`. Only one request was
    # ever sent (the refusal sends nothing).
    completed, posts = _run(
        daimonos_binary, tmp_path, wire, [turn([final_call({"answer": 42})])],
        corrective_retries=5, max_model_requests=1)
    assert completed.returncode == 24, completed.stderr
    payload = _last_json(completed.stderr)
    assert payload["reason"] == "turn_limit"
    assert payload["model_requests"] == 1
    assert posts == 1


@BOTH_WIRES
def test_success_on_the_last_allowed_request(daimonos_binary, tmp_path, wire):
    # max_model_requests=2: an invalid first ending retries, and the valid final
    # answer on the second (last permitted) request succeeds.
    turns = [turn([final_call({"answer": 42})]), turn([final_call({"answer": "done"})])]
    completed, posts = _run(daimonos_binary, tmp_path, wire, turns,
                            corrective_retries=1, max_model_requests=2)
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == '{"answer":"done"}\n'
    assert posts == 2


@BOTH_WIRES
def test_r4_fallback_reports_real_counters(daimonos_binary, tmp_path, wire):
    # R4: a MaxTokens-truncated evidence turn reaches no explicit final-output
    # decision (auto-continue is disabled in final-output mode), so the run
    # falls back to no_final_call. The reported model_requests must be the real
    # count (1), never 0.
    turns = [turn([{"id": "r1", "name": "read_file",
                    "args": {"path": "evidence.txt"}}], stop="max_tokens")]
    completed, posts = _run(daimonos_binary, tmp_path, wire, turns,
                            corrective_retries=0,
                            extra_files={"evidence.txt": "x"})
    assert completed.returncode == 20, completed.stderr
    payload = _last_json(completed.stderr)
    assert payload["reason"] == "no_final_call"
    assert payload["model_requests"] == 1, payload
    assert posts == 1


# --- Startup refusals (wire-independent; reached before any model request) ---


def test_ac_01_6_external_reference_is_schema_unusable(daimonos_binary, tmp_path):
    completed, posts = _run(
        daimonos_binary, tmp_path, "chat", [turn([final_call({"answer": "x"})])],
        schema={"$ref": "file:///etc/passwd"})
    assert completed.returncode == 20, completed.stderr
    assert _last_json(completed.stderr)["reason"] == "schema_unusable"
    assert posts == 0, "no model request may be made for an unusable schema"


def test_mutually_exclusive_schema_fields_fail_startup(daimonos_binary, tmp_path):
    # Hermetic (empty HOME, no agent env): the control-file structural error is
    # validated BEFORE the agent env file is read, so it exits 2 even with no
    # agent.env present (CI-1 / INTERFACES startup order).
    home = tmp_path / "home"
    home.mkdir()
    control = _write_control(
        tmp_path, schema=SCHEMA, schema_path="other.json",
        tool_name="final_answer", tool_description="finish", corrective_retries=1)
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path), "agent",
         "--final-output", control, "--print", "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    assert completed.returncode == 2, completed.stderr


def test_tool_name_collision_fails_startup(daimonos_binary, tmp_path):
    # Hermetic: the collision is detected before the agent env file is read.
    home = tmp_path / "home"
    home.mkdir()
    control = _write_control(
        tmp_path, schema=SCHEMA, tool_name="read_file",
        tool_description="finish", corrective_retries=0)
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path), "agent",
         "--final-output", control, "--print", "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    assert completed.returncode == 2, completed.stderr


def test_default_off_leaves_agent_output_on_stdout(daimonos_binary, tmp_path):
    # Opt-out regression (ADR-024: default off = behavior unchanged). Without
    # --final-output the one-shot agent runs normally: the assistant's own text
    # reaches stdout and no final-output JSON / typed error is produced. The
    # stdout-suppression path is gated on the flag being active, so omitting it
    # must leave the ordinary echo in place.
    completed, posts = _run(
        daimonos_binary, tmp_path, "chat",
        [turn(text="plain assistant reply")], final_output=False)
    assert completed.returncode == 0, completed.stderr
    assert "plain assistant reply" in completed.stdout, repr(completed.stdout)
    assert posts >= 1
    # The flag stays optional: it is still advertised but not required.
    assert "--final-output" in _agent_help(daimonos_binary)


# --- J4: per-frontend coverage of --final-output -----------------------------
#
# `--final-output` is honored only by `agent --print` (the one-shot path). Every
# other frontend that cannot honor it refuses it at startup with a named error
# (exit 2, INTERFACES.md `run_configuration_invalid`), never silently ignoring
# it:
#   * interactive agent  -> clap conflicts_with "interactive"
#   * dry-run agent      -> clap conflicts_with "dry_run"
#   * chat / acp / daemon-> the flag is agent-only, so clap rejects it as an
#                           unexpected argument on those subcommands.


def _control(tmp_path):
    return _write_control(tmp_path, schema=SCHEMA, tool_name="final_answer",
                          tool_description="finish", corrective_retries=0)


def test_final_output_refused_in_interactive_mode(daimonos_binary, tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path), "agent",
         "--interactive", "--final-output", _control(tmp_path), "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "--final-output" in completed.stderr
    assert "interactive" in completed.stderr.lower()


def test_final_output_refused_in_dry_run_mode(daimonos_binary, tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path), "agent",
         "--dry-run", "--final-output", _control(tmp_path), "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "--final-output" in completed.stderr


@pytest.mark.parametrize("subcommand", ["chat", "acp", "session-daemon"])
def test_final_output_rejected_by_non_agent_frontend(daimonos_binary, tmp_path, subcommand):
    # chat, ACP, and the session daemon never carry --final-output; clap refuses
    # the unexpected argument at startup (exit 2) rather than ignoring it.
    home = tmp_path / "home"
    home.mkdir()
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path), subcommand,
         "--final-output", _control(tmp_path)],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    assert completed.returncode == 2, completed.stdout + completed.stderr
    assert "--final-output" in completed.stderr
    assert completed.stdout == ""


# --- J5: stdout carries only the one JSON document in every outcome ----------


@BOTH_WIRES
def test_stdout_empty_on_text_only_failure(daimonos_binary, tmp_path, wire):
    # A failed ending whose model turn emitted prose must leave stdout empty:
    # the typed error goes to stderr and the assistant text is never echoed.
    completed, _ = _run(
        daimonos_binary, tmp_path, wire,
        [turn(text="Here is a long prose answer, deliberately not a tool call.")],
        corrective_retries=0)
    assert completed.returncode == 20, completed.stderr
    assert completed.stdout == "", repr(completed.stdout)
    assert _last_json(completed.stderr)["reason"] == "no_final_call"
