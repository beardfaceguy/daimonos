"""One-shot `daimonos agent` options of ADR-036 (vikunja #1621).

These tests run the real binary against a local OpenRouter-compatible endpoint.
No request leaves 127.0.0.1.
"""
import json
import shutil
import sqlite3
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest


def text_reply(text, usage=None):
    return {"kind": "message", "message": {"role": "assistant", "content": text},
            "finish_reason": "stop", "usage": usage}


def tool_reply(name, arguments):
    call = {"id": "call-1", "type": "function",
            "function": {"name": name, "arguments": json.dumps(arguments)}}
    return {"kind": "message", "message": {"role": "assistant", "content": "", "tool_calls": [call]},
            "finish_reason": "tool_calls", "usage": None}


def http_error(status, message):
    return {"kind": "error", "status": status, "message": message}


class FakeProvider:
    """Serve scripted chat completions and record each request body."""

    def __init__(self, replies):
        self.replies = list(replies)
        self.requests = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def _send(self, status, content_type, payload):
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                self.wfile.write(payload)
                self.wfile.flush()

            def do_GET(self):
                models = {"data": [{"id": "test/model", "context_length": 100000,
                                    "supported_parameters": ["tools"],
                                    "pricing": {"prompt": "0", "completion": "0"}}]}
                self._send(200, "application/json", json.dumps(models).encode())

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                owner.requests.append(body)
                reply = owner.replies.pop(0) if owner.replies else http_error(500, "fake provider has no reply")
                gate = reply.get("hold")
                if gate is not None:
                    # Keep the run in flight until the test releases it.
                    assert gate.wait(timeout=30), "the test did not release a held reply"
                if reply["kind"] == "error":
                    payload = json.dumps({"error": {"message": reply["message"]}}).encode()
                    self._send(reply["status"], "application/json", payload)
                    return
                usage = reply["usage"] or {"prompt_tokens": 1, "completion_tokens": 1}
                if body.get("stream"):
                    chunk = {"choices": [{"index": 0, "delta": reply["message"],
                                          "finish_reason": reply["finish_reason"]}], "usage": usage}
                    payload = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
                    self._send(200, "text/event-stream", payload)
                else:
                    document = {"choices": [{"index": 0, "message": reply["message"],
                                             "finish_reason": reply["finish_reason"]}], "usage": usage}
                    self._send(200, "application/json", json.dumps(document).encode())

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


@pytest.fixture
def fake_provider():
    providers = []

    def start(replies):
        provider = FakeProvider(replies)
        providers.append(provider)
        return provider

    yield start
    for provider in providers:
        provider.close()


class Harness:
    """One isolated HOME, workspace, and agent env for `daimonos agent` runs."""

    def __init__(self, binary, tmp_path, port):
        self.binary = binary
        self.workspace = tmp_path / "workspace"
        self.workspace.mkdir()
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.config = tmp_path / "config.toml"
        self.config.write_text(
            "[analytics]\nenabled=false\n"
            f'[logging]\nlevel="warn"\ndirectory="{tmp_path / "logs"}"\n'
        )
        self.agent_env = tmp_path / "agent.env"
        self.agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{port}", "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_COMPACTION=off", ""]))

    def command(self, *args, global_args=()):
        return [self.binary, "--workspace", str(self.workspace), "--config", str(self.config),
                *global_args, "agent", "--agent-env", str(self.agent_env), *args]

    def environment(self):
        return {"HOME": str(self.home), "PATH": "/usr/bin:/bin"}

    def agent(self, *args, global_args=()):
        return subprocess.run(
            self.command(*args, global_args=global_args), cwd=self.workspace, env=self.environment(),
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)

    def start(self, *args):
        """Start a run and return at once. The caller reads it with `communicate`."""
        return subprocess.Popen(
            self.command(*args), cwd=self.workspace, env=self.environment(),
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def only_json_document(stdout):
    """Stdout must be exactly one JSON document and one newline."""
    assert stdout.endswith("\n"), repr(stdout)
    assert "\n" not in stdout[:-1], repr(stdout)
    return json.loads(stdout)


def test_text_output_is_the_assistant_text_without_the_new_options(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("plain answer")])
    done = Harness(daimonos_binary, tmp_path, provider.port).agent("say something")
    assert done.returncode == 0, done.stderr
    assert done.stdout == "plain answer\n"


def test_output_json_is_the_only_stdout_content_when_a_tool_child_prints(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([
        tool_reply("exec", {"command": "echo", "args": ["CHILD_STDOUT_MARKER"]}),
        text_reply("finished", usage={"prompt_tokens": 120, "completion_tokens": 9,
                                      "prompt_tokens_details": {"cached_tokens": 100}}),
    ])
    done = Harness(daimonos_binary, tmp_path, provider.port).agent("--output", "json", "run echo")
    assert done.returncode == 0, done.stderr

    doc = only_json_document(done.stdout)
    assert "CHILD_STDOUT_MARKER" not in done.stdout
    # The tool ran: its output reached the model as a tool result.
    assert "CHILD_STDOUT_MARKER" in json.dumps(provider.requests[1]["messages"])
    assert doc["schema_version"] == 1
    assert doc["provider"] == "openrouter"
    assert doc["model"] == "test/model"
    assert doc["stop_reason"] == "end_turn"
    assert doc["final_text"] == "finished"
    assert doc["session_id"] is None and doc["session_saved"] is None and doc["resumed"] is False
    # Two calls: (1 + 1) and (20 non-cached + 100 cached, 9 output).
    assert doc["usage"]["input_tokens"] == 21
    assert doc["usage"]["output_tokens"] == 10
    assert doc["usage"]["cache_read_tokens"] == 100


def test_output_json_reports_a_provider_error_and_exits_non_zero(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([http_error(400, "bad request from the fake provider")])
    done = Harness(daimonos_binary, tmp_path, provider.port).agent("--output", "json", "go")
    assert done.returncode == 1, done.stderr

    doc = only_json_document(done.stdout)
    assert doc["stop_reason"] == "error"
    assert "bad request from the fake provider" in doc["error_message"]
    assert doc["context_overflow"] is False
    assert doc["final_text"] == ""


def test_output_json_reports_a_context_overflow(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([http_error(400, "This model's maximum context length is 10 tokens.")])
    done = Harness(daimonos_binary, tmp_path, provider.port).agent("--output", "json", "go")
    assert done.returncode == 1, done.stderr

    doc = only_json_document(done.stdout)
    assert doc["stop_reason"] == "error"
    assert doc["context_overflow"] is True
    # No compaction and no retry: one request reached the provider.
    assert len(provider.requests) == 1


# --- named sessions (--session-id) ---

def refusal(done):
    """A refused run: exit 2, nothing on stdout, one JSON error on stderr."""
    assert done.returncode == 2, (done.returncode, done.stderr)
    assert done.stdout == ""
    errors = [json.loads(line) for line in done.stderr.splitlines() if line.startswith("{")]
    assert len(errors) == 1, done.stderr
    assert errors[0]["error"]["code"] == "run_configuration_invalid"
    return errors[0]["error"]


def session_rows(database):
    with sqlite3.connect(database) as db:
        return db.execute("SELECT session_id, generation, payload FROM sessions ORDER BY session_id").fetchall()


def test_session_id_saves_a_session_and_refuses_the_same_id_again(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("FIRST"), text_reply("NEVER SENT")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    database = harness.home / ".daimonos" / "oneshot-sessions" / "sessions.sqlite3"

    first = harness.agent("--session-id", "run-1", "first task")
    assert first.returncode == 0, first.stderr
    assert first.stdout == "FIRST\n"
    rows = session_rows(database)
    assert [(row[0], row[1]) for row in rows] == [("run-1", 1)]
    record = json.loads(rows[0][2])
    assert record["model"] == "test/model"
    assert record["prefix"]["provider"] == "openrouter"
    assert [message["role"].lower() for message in record["messages"]] == ["user", "assistant"]
    # chat does not list a one-shot session.
    assert not (harness.home / ".daimonos" / "chat-sessions").exists()

    second = harness.agent("--session-id", "run-1", "second task")
    error = refusal(second)
    assert error["reason"] == "session_exists"
    assert len(provider.requests) == 1, "a refused run must not call the provider"
    assert session_rows(database) == rows, "a refused run must not change the record"


def test_an_invalid_session_id_is_refused_before_any_store_access(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("NEVER SENT")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    for bad in ["has space", "dot.dot", "a" * 129]:
        error = refusal(harness.agent("--session-id", bad, "task"))
        assert error["reason"] == "session_id_invalid"
    assert provider.requests == []
    assert not (harness.home / ".daimonos" / "oneshot-sessions").exists()


def test_a_selected_state_root_holds_the_session_store(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("FIRST")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    # A selected state root refuses a log directory outside it, so this run
    # uses the default log placement under the state root.
    harness.config.write_text("[analytics]\nenabled=false\n")

    done = harness.agent("--session-id", "run-1", "first task", global_args=("--state-dir", str(state)))
    assert done.returncode == 0, done.stderr
    assert [row[0] for row in session_rows(state / "sessions" / "oneshot" / "sessions.sqlite3")] == ["run-1"]
    assert not (harness.home / ".daimonos" / "oneshot-sessions").exists()


def test_output_json_names_the_saved_session(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("FIRST")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    done = harness.agent("--session-id", "run-1", "--output", "json", "first task")
    assert done.returncode == 0, done.stderr
    doc = only_json_document(done.stdout)
    assert (doc["session_id"], doc["resumed"], doc["session_saved"], doc["session_error"]) == ("run-1", False, True, None)


# --- resumed sessions (--resume) ---

def held(reply):
    """A reply that the provider sends only after the returned event is set."""
    gate = threading.Event()
    return dict(reply, hold=gate), gate


def wait_for_requests(provider, count):
    deadline = time.monotonic() + 30
    while len(provider.requests) < count:
        assert time.monotonic() < deadline, f"the provider received {len(provider.requests)} request(s)"
        time.sleep(0.02)


def ordered(value):
    """Serialize with the received key order, so equality also compares order."""
    return json.dumps(value)


def add_workspace_skill(harness):
    """Change the workspace so that a rebuilt system prompt differs."""
    skill = harness.workspace / ".agents" / "skills" / "probe"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: probe\ndescription: resume prefix probe\n---\nbody\n")


def test_resume_sends_the_same_prefix_and_only_the_new_input(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("FIRST"), text_reply("SECOND"), text_reply("CONTROL")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)

    first = harness.agent("--session-id", "run-1", "first task")
    assert first.returncode == 0, first.stderr
    add_workspace_skill(harness)
    second = harness.agent("--resume", "run-1", "the tests failed: E1")
    assert second.returncode == 0, second.stderr
    # Only the assistant text of this invocation.
    assert second.stdout == "SECOND\n"
    # Control: a new session in the changed workspace gets another system prompt.
    control = harness.agent("--session-id", "run-2", "first task")
    assert control.returncode == 0, control.stderr

    one, two, three = provider.requests
    assert ordered(three["messages"][0]) != ordered(one["messages"][0]), \
        "control: the workspace change must alter a rebuilt system prompt"
    assert one["messages"][0]["role"] == "system"
    assert ordered(two["tools"]) == ordered(one["tools"])
    earlier = len(one["messages"])
    assert ordered(two["messages"][:earlier]) == ordered(one["messages"])
    added = two["messages"][earlier:]
    assert [message["role"] for message in added] == ["assistant", "user"]
    assert "FIRST" in ordered(added[0])
    assert "the tests failed: E1" in ordered(added[1])
    assert two["model"] == one["model"]


def test_resume_refuses_another_model_another_provider_and_an_unknown_id(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("FIRST"), text_reply("NEVER SENT")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    assert harness.agent("--session-id", "run-1", "first task").returncode == 0
    database = harness.home / ".daimonos" / "oneshot-sessions" / "sessions.sqlite3"
    before = session_rows(database)

    model = refusal(harness.agent("--resume", "run-1", "--model", "other/model", "more"))
    assert model["reason"] == "session_model_mismatch"
    assert "test/model" in model["message"] and "other/model" in model["message"]
    other_provider = refusal(harness.agent("--resume", "run-1", "--provider", "openai", "more"))
    assert other_provider["reason"] == "session_provider_mismatch"
    unknown = refusal(harness.agent("--resume", "run-9", "more"))
    assert unknown["reason"] == "session_not_found"

    assert len(provider.requests) == 1, "a refused run must not call the provider"
    assert session_rows(database) == before
    # The saved model, named explicitly, is accepted.
    same = harness.agent("--resume", "run-1", "--model", "test/model", "more")
    assert same.returncode == 0, same.stderr


def test_a_second_resume_of_a_running_session_is_refused(daimonos_binary, tmp_path, fake_provider):
    slow, gate = held(text_reply("SECOND"))
    provider = fake_provider([text_reply("FIRST"), slow, text_reply("NEVER SENT")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    assert harness.agent("--session-id", "run-1", "first task").returncode == 0

    running = harness.start("--resume", "run-1", "second task")
    try:
        wait_for_requests(provider, 2)
        error = refusal(harness.agent("--resume", "run-1", "third task"))
        assert error["reason"] == "session_owned_elsewhere"
        assert str(running.pid) in error["message"]
    finally:
        gate.set()
        stdout, stderr = running.communicate(timeout=30)
    assert running.returncode == 0, stderr
    assert stdout == "SECOND\n"
    assert len(provider.requests) == 2, "the refused run must not call the provider"
    database = harness.home / ".daimonos" / "oneshot-sessions" / "sessions.sqlite3"
    assert [(row[0], row[1]) for row in session_rows(database)] == [("run-1", 2)]


def test_a_failed_save_exits_non_zero_after_the_json_document(daimonos_binary, tmp_path, fake_provider):
    slow, gate = held(text_reply("FIRST"))
    provider = fake_provider([slow])
    harness = Harness(daimonos_binary, tmp_path, provider.port)

    running = harness.start("--session-id", "run-1", "--output", "json", "first task")
    try:
        wait_for_requests(provider, 1)
        # Remove the store while the run is in flight, so the save cannot find its writer.
        shutil.rmtree(harness.home / ".daimonos" / "oneshot-sessions")
    finally:
        gate.set()
        stdout, stderr = running.communicate(timeout=30)
    assert running.returncode == 1, (running.returncode, stderr)
    doc = only_json_document(stdout)
    assert doc["stop_reason"] == "end_turn"
    assert (doc["session_id"], doc["session_saved"]) == ("run-1", False)
    assert doc["session_error"]
    assert "session run-1 was not saved" in stderr


def test_resume_json_reports_this_invocation_only(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([
        text_reply("FIRST", usage={"prompt_tokens": 900, "completion_tokens": 50}),
        text_reply("SECOND", usage={"prompt_tokens": 1000, "completion_tokens": 7,
                                    "prompt_tokens_details": {"cached_tokens": 940}}),
    ])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    assert harness.agent("--session-id", "run-1", "first task").returncode == 0
    done = harness.agent("--resume", "run-1", "--output", "json", "second task")
    assert done.returncode == 0, done.stderr
    doc = only_json_document(done.stdout)
    assert (doc["session_id"], doc["resumed"], doc["session_saved"]) == ("run-1", True, True)
    assert doc["final_text"] == "SECOND"
    assert doc["turns"] == 1
    assert doc["usage"]["input_tokens"] == 60
    assert doc["usage"]["cache_read_tokens"] == 940
    assert doc["usage"]["output_tokens"] == 7


# --- edge cases of the plan ("Error and edge behavior") ---

def test_a_refused_run_prints_no_json_document(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("NEVER SENT")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    # `refusal` checks that stdout is empty: the run did not start, so it has no document.
    error = refusal(harness.agent("--resume", "run-9", "--output", "json", "more"))
    assert error["reason"] == "session_not_found"
    assert provider.requests == []


def test_a_session_can_be_resumed_after_a_failed_run(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([http_error(400, "bad request from the fake provider"), text_reply("RECOVERED")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)

    failed = harness.agent("--session-id", "run-1", "--output", "json", "first task")
    assert failed.returncode == 1, failed.stderr
    doc = only_json_document(failed.stdout)
    assert (doc["stop_reason"], doc["session_saved"]) == ("error", True)
    # The failed first run consumed the id (ADR-036 Decision 8).
    assert refusal(harness.agent("--session-id", "run-1", "first task"))["reason"] == "session_exists"

    resumed = harness.agent("--resume", "run-1", "try again")
    assert resumed.returncode == 0, resumed.stderr
    assert resumed.stdout == "RECOVERED\n"
    # The failed call left no assistant content. The saved history ends with
    # the first user message, so the resumed request has two user messages in
    # sequence and no empty assistant message.
    resumed_messages = provider.requests[-1]["messages"]
    assert [message["role"] for message in resumed_messages] == ["system", "user", "user"]
    assert "first task" in ordered(resumed_messages[1])
    assert "try again" in ordered(resumed_messages[2])


def test_a_killed_run_saves_nothing_and_frees_the_id(daimonos_binary, tmp_path, fake_provider):
    slow, gate = held(text_reply("NEVER RECEIVED"))
    provider = fake_provider([slow, text_reply("AFTER KILL")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)

    running = harness.start("--session-id", "run-1", "first task")
    try:
        wait_for_requests(provider, 1)
        running.kill()
        running.communicate(timeout=30)
    finally:
        gate.set()
    database = harness.home / ".daimonos" / "oneshot-sessions" / "sessions.sqlite3"
    assert session_rows(database) == [], "a killed run must not leave a record"

    again = harness.agent("--session-id", "run-1", "first task")
    assert again.returncode == 0, again.stderr
    assert again.stdout == "AFTER KILL\n"
