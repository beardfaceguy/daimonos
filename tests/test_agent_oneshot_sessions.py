"""One-shot `daimonos agent` options of ADR-036 (vikunja #1621).

These tests run the real binary against a local OpenRouter-compatible endpoint.
No request leaves 127.0.0.1.
"""
import json
import subprocess
import threading
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

    def agent(self, *args, global_args=()):
        return subprocess.run(
            [self.binary, "--workspace", str(self.workspace), "--config", str(self.config),
             *global_args, "agent", "--agent-env", str(self.agent_env), *args],
            cwd=self.workspace, env={"HOME": str(self.home), "PATH": "/usr/bin:/bin"},
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)


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
