"""Process lifecycle of `daimonos acp`: stdin EOF and termination signals.

Each process runs with an empty HOME and its own workspace. A developer host
can have a `~/.config/daimonos/agent.env`, and a test must not read it or send
requests to the provider that it names (vikunja #1623).
"""
import json
import signal
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

SIGNALS = [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]


def _spawn_acp(daimonos_binary, tmp_path, base_url="http://127.0.0.1:1"):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        f"DAIMONOS_AGENT_BASE_URL={base_url}",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    return subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "acp", "--agent-env", str(agent_env)],
        cwd=workspace,
        env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def _stop(process):
    if process.poll() is None:
        process.kill()
        process.wait()


def _initialize(process):
    """Complete the ACP handshake. A reply shows that the transport loop runs."""
    request = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": 1, "clientCapabilities": {}}}
    process.stdin.write((json.dumps(request) + "\n").encode())
    process.stdin.flush()
    reply = json.loads(process.stdout.readline())
    assert reply["id"] == 1 and "result" in reply, reply


def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
    process = _spawn_acp(daimonos_binary, tmp_path)

    process.stdin.close()

    try:
        assert process.wait(timeout=5) == 0
    finally:
        _stop(process)


@pytest.mark.parametrize("sig", SIGNALS)
def test_acp_runs_teardown_when_signaled(daimonos_binary, tmp_path, sig):
    """Signals take the EOF teardown path instead of the default handler (#1544).

    The default handler exits with -signal and skips bridge/tool teardown,
    orphaning running tool process groups; a clean 0 proves teardown ran.
    """
    process = _spawn_acp(daimonos_binary, tmp_path)
    try:
        _initialize(process)
        process.send_signal(sig)
        assert process.wait(timeout=10) == 0
    finally:
        _stop(process)


class HeldCatalog:
    """A provider endpoint that holds its model-catalog reply until released."""

    def __init__(self):
        owner = self
        self.arrived = threading.Event()
        self.release = threading.Event()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.arrived.set()
                owner.release.wait(timeout=30)
                body = json.dumps({"data": [{"id": "test/model", "context_length": 100000}]}).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except OSError:
                    pass  # the client is gone

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


@pytest.mark.parametrize("sig", SIGNALS)
def test_acp_exits_cleanly_when_signaled_during_startup(daimonos_binary, tmp_path, sig):
    """A signal that arrives while startup waits for the provider ends the
    process with code 0, and at once (#1623).

    Startup asks the provider for its model catalog. Before the fix, the signal
    handlers were installed after that request, so a signal during the request
    used the default action: exit by signal, with no teardown.
    """
    catalog = HeldCatalog()
    process = _spawn_acp(daimonos_binary, tmp_path, base_url=catalog.url)
    try:
        assert catalog.arrived.wait(timeout=10), "startup did not request the model catalog"
        process.send_signal(sig)
        # The catalog reply is still held: the exit does not wait for it. The
        # limit is below the 5 s that startup waits for the catalog.
        assert process.wait(timeout=4) == 0
        assert not catalog.release.is_set()
    finally:
        _stop(process)
        catalog.close()
