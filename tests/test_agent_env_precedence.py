"""Precedence of agent settings: real environment > `--agent-env` file > implicit files.

The implicit files are `<workspace>/agent.env` and `~/.config/daimonos/agent.env`.
Daimonos loads them into the process environment at startup. Before vikunja
#1623 their values then counted as process values and replaced the values of
the file that `--agent-env` names.

Each test uses two fake provider endpoints. The endpoint that receives the
model request shows which file supplied `DAIMONOS_AGENT_BASE_URL`.
"""
import json
import os
import pty
import signal
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from test_agent_oneshot_sessions import Harness, fake_provider, only_json_document, text_reply

__all__ = ["fake_provider"]  # the fixture of the imported module


def settings(port, model):
    return "\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter", f"DAIMONOS_AGENT_MODEL={model}",
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{port}", "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_COMPACTION=off", ""])


def write_home_agent_env(harness, port, model="home/model"):
    directory = harness.home / ".config" / "daimonos"
    directory.mkdir(parents=True)
    (directory / "agent.env").write_text(settings(port, model))


def run(harness, argv, extra_env=None):
    return subprocess.run(
        argv, cwd=harness.workspace, env={**harness.environment(), **(extra_env or {})},
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)


def test_explicit_agent_env_wins_over_the_home_agent_env(daimonos_binary, tmp_path, fake_provider):
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)

    done = harness.agent("--output", "json", "task")
    assert done.returncode == 0, done.stderr
    doc = only_json_document(done.stdout)
    assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE EXPLICIT FILE")
    assert implicit.requests == [], "the run used the endpoint of the home agent.env"
    assert len(explicit.requests) == 1


def test_explicit_agent_env_wins_over_the_workspace_agent_env_with_a_state_dir(
        daimonos_binary, tmp_path, fake_provider):
    # A run with --state-dir does not read the home file. It reads <workspace>/agent.env.
    implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    harness.config.write_text("[analytics]\nenabled=false\n")
    (harness.workspace / "agent.env").write_text(settings(implicit.port, "workspace/model"))
    state = tmp_path / "state"
    state.mkdir(mode=0o700)

    done = harness.agent("--output", "json", "task", global_args=("--state-dir", str(state)))
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "test/model"
    assert implicit.requests == []
    assert len(explicit.requests) == 1


def test_a_file_from_the_real_selector_variable_wins_over_the_home_agent_env(
        daimonos_binary, tmp_path, fake_provider):
    # DAIMONOS_AGENT_ENV in the real environment selects the file, as --agent-env does.
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE SELECTED FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)

    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
            "agent", "--output", "json", "task"]
    done = run(harness, argv, {"DAIMONOS_AGENT_ENV": str(harness.agent_env)})
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "test/model"
    assert implicit.requests == []
    assert len(explicit.requests) == 1


def test_a_file_that_the_workspace_agent_env_selects_is_not_an_explicit_file(
        daimonos_binary, tmp_path, fake_provider):
    # Unchanged behavior: an implicit file can name the agent env file. It then
    # keeps its rank above that file, so its own values win.
    implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
    selected = fake_provider([text_reply("FROM THE SELECTED FILE")])
    harness = Harness(daimonos_binary, tmp_path, selected.port)
    (harness.workspace / "agent.env").write_text(
        f"DAIMONOS_AGENT_ENV={harness.agent_env}\n"
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{implicit.port}\n")

    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
            "agent", "--output", "json", "task"]
    done = run(harness, argv)
    assert done.returncode == 0, done.stderr
    doc = only_json_document(done.stdout)
    assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE WORKSPACE FILE")
    assert selected.requests == []


def test_the_real_environment_wins_over_the_explicit_agent_env(daimonos_binary, tmp_path, fake_provider):
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)

    done = run(harness, harness.command("--output", "json", "task"),
               {"DAIMONOS_AGENT_MODEL": "environment/model"})
    assert done.returncode == 0, done.stderr
    # The model comes from the environment. The endpoint comes from the explicit file.
    assert only_json_document(done.stdout)["model"] == "environment/model"
    assert implicit.requests == []
    assert len(explicit.requests) == 1


def test_the_home_agent_env_supplies_a_value_that_the_explicit_file_omits(
        daimonos_binary, tmp_path, fake_provider):
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)
    lines = [line for line in harness.agent_env.read_text().splitlines()
             if not line.startswith("DAIMONOS_AGENT_MODEL=")]
    harness.agent_env.write_text("\n".join(lines) + "\n")

    done = harness.agent("--output", "json", "task")
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "home/model"
    assert implicit.requests == []
    assert len(explicit.requests) == 1


def test_the_home_agent_env_is_the_selected_file_without_an_explicit_file(
        daimonos_binary, tmp_path, fake_provider):
    # Control: without --agent-env nothing changes. The home file is the selected file.
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    harness = Harness(daimonos_binary, tmp_path, 1)
    write_home_agent_env(harness, implicit.port)

    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
            "agent", "--output", "json", "task"]
    done = run(harness, argv)
    assert done.returncode == 0, done.stderr
    doc = only_json_document(done.stdout)
    assert (doc["model"], doc["final_text"]) == ("home/model", "FROM THE HOME FILE")
    assert len(implicit.requests) == 1


class CatalogEndpoint:
    """A provider endpoint that answers the model-catalog request and counts it."""

    def __init__(self):
        owner = self
        self.requested = threading.Event()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.requested.set()
                body = json.dumps({"data": [{"id": "test/model", "context_length": 100000}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.port = self.server.server_address[1]

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def test_the_session_daemon_of_an_interactive_run_uses_the_explicit_agent_env(daimonos_binary, tmp_path):
    """`agent --interactive` starts the session daemon as a child process. The
    daemon asks its provider for the model catalog at startup. That request
    must go to the endpoint of the `--agent-env` file, not to the endpoint of
    the home `agent.env` that the parent process loaded.
    """
    implicit, explicit = CatalogEndpoint(), CatalogEndpoint()
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port, model="test/model")
    socket_path = tmp_path / "daemon.sock"
    harness.config.write_text(
        "[analytics]\nenabled=false\n"
        f'[session]\nsocket_path = "{socket_path}"\nbootstrap_timeout_secs = 10\n'
        "bootstrap_retry_interval_ms = 10\n")
    instance_path = tmp_path / "daemon.sock.pid"
    master_fd, slave_fd = pty.openpty()
    process = subprocess.Popen(
        harness.command("--interactive"), cwd=harness.workspace,
        env={**harness.environment(), "TERM": "xterm"},
        stdin=slave_fd, stdout=slave_fd, stderr=subprocess.PIPE, text=True)
    os.close(slave_fd)
    try:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and not (explicit.requested.is_set() or implicit.requested.is_set()):
            assert process.poll() is None, process.stderr.read()
            time.sleep(0.02)
        assert not implicit.requested.is_set(), "the daemon used the endpoint of the home agent.env"
        assert explicit.requested.is_set(), "the daemon requested no model catalog"
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        deadline = time.monotonic() + 10
        while not instance_path.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        if instance_path.exists():
            try:
                os.kill(json.loads(instance_path.read_text())["pid"], signal.SIGTERM)
            except (ProcessLookupError, ValueError):
                pass
            deadline = time.monotonic() + 5
            while socket_path.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
        os.close(master_fd)
        implicit.close()
        explicit.close()
    assert not socket_path.exists(), "the test left a session daemon"
