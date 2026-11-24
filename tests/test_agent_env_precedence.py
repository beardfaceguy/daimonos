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


def without(harness, name):
    """Remove one setting from the explicit file of the harness."""
    lines = [line for line in harness.agent_env.read_text().splitlines() if not line.startswith(name + "=")]
    harness.agent_env.write_text("\n".join(lines) + "\n")


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


def test_a_file_that_an_implicit_file_selects_is_not_an_explicit_file(
        daimonos_binary, tmp_path, fake_provider):
    # The user agent.env can name the agent env file. That is not an explicit
    # selection, so a setting of the user file still overrides the file that it
    # names. (The workspace agent.env cannot name a file: ADR-038.)
    provider = fake_provider([text_reply("REPLY")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    directory = harness.home / ".config" / "daimonos"
    directory.mkdir(parents=True)
    (directory / "agent.env").write_text(
        f"DAIMONOS_AGENT_ENV={harness.agent_env}\nDAIMONOS_AGENT_MODEL=user/model\n")

    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
            "agent", "--output", "json", "task"]
    done = run(harness, argv)
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "user/model"


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
    without(harness, "DAIMONOS_AGENT_MODEL")

    done = harness.agent("--output", "json", "task")
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "home/model"
    assert implicit.requests == []
    assert len(explicit.requests) == 1


def test_the_api_key_of_the_home_agent_env_is_not_sent_to_the_endpoint_of_the_explicit_file(
        daimonos_binary, tmp_path, fake_provider):
    # The explicit file names an endpoint and has no API key. The key of the
    # home file must not go to that endpoint: the run stops before a request.
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)
    home_file = harness.home / ".config" / "daimonos" / "agent.env"
    home_file.write_text(home_file.read_text().replace("API_KEY=test", "API_KEY=sk-home-secret"))
    without(harness, "DAIMONOS_AGENT_API_KEY")

    done = harness.agent("--output", "json", "task")
    assert done.returncode != 0
    assert done.stdout == ""
    assert "DAIMONOS_AGENT_API_KEY" in done.stderr
    assert "Not used from an implicit agent.env file" in done.stderr
    assert "sk-home-secret" not in done.stderr
    assert implicit.requests == [] and explicit.requests == []


def test_the_endpoint_of_the_home_agent_env_does_not_get_the_api_key_of_the_explicit_file(
        daimonos_binary, tmp_path, fake_provider):
    # The reverse: the explicit file has a key and no base URL.
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)
    without(harness, "DAIMONOS_AGENT_BASE_URL")

    done = harness.agent("--output", "json", "task")
    assert done.returncode != 0
    assert "DAIMONOS_AGENT_BASE_URL" in done.stderr
    assert implicit.requests == [] and explicit.requests == []


def test_the_real_environment_can_supply_a_connection_setting_for_the_explicit_file(
        daimonos_binary, tmp_path, fake_provider):
    implicit = fake_provider([text_reply("FROM THE HOME FILE")])
    explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port)
    without(harness, "DAIMONOS_AGENT_API_KEY")

    done = run(harness, harness.command("--output", "json", "task"), {"DAIMONOS_AGENT_API_KEY": "from-the-launcher"})
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["final_text"] == "FROM THE EXPLICIT FILE"
    assert implicit.requests == []


def test_explicit_agent_env_wins_over_the_workspace_agent_env_under_confinement(
        daimonos_binary, tmp_path, fake_provider):
    # With --confine-paths, only <workspace>/agent.env is an implicit file, and
    # an agent env file must be named. Both ways to name it are explicit.
    for index, by_option in enumerate([True, False]):
        implicit = fake_provider([text_reply("FROM THE WORKSPACE FILE")])
        explicit = fake_provider([text_reply("FROM THE EXPLICIT FILE")])
        harness = Harness(daimonos_binary, tmp_path / str(index), explicit.port)
        (harness.workspace / "agent.env").write_text(settings(implicit.port, "workspace/model"))
        state = tmp_path / str(index) / "state"
        state.mkdir(mode=0o700)
        argv = [harness.binary, "--workspace", str(harness.workspace), "--state-dir", str(state),
                "--confine-paths", "agent", "--output", "json"]
        extra_env = {}
        if by_option:
            argv += ["--agent-env", str(harness.agent_env)]
        else:
            extra_env["DAIMONOS_AGENT_ENV"] = str(harness.agent_env)
        done = run(harness, argv + ["task"], extra_env)
        assert done.returncode == 0, (by_option, done.stderr)
        doc = only_json_document(done.stdout)
        assert (doc["model"], doc["final_text"]) == ("test/model", "FROM THE EXPLICIT FILE"), by_option
        assert implicit.requests == [], by_option


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


def session_daemon_pids(socket_path):
    """The session daemon processes that serve `socket_path`, found by command line."""
    wanted = str(socket_path).encode()
    pids = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as handle:
                argv = handle.read().split(b"\0")
        except OSError:
            continue  # the process ended
        if b"session-daemon" in argv and wanted in argv:
            pids.append(int(entry))
    return pids


def stop_session_daemons(socket_path, grace=20.0):
    """Stop each session daemon of `socket_path`, and wait until it is gone.

    The daemon is not a child of the test, and it does not depend on its pid
    file here: a daemon that is still in its startup has not written one.
    """
    for pid in session_daemon_pids(socket_path):
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + grace
    while session_daemon_pids(socket_path) and time.monotonic() < deadline:
        time.sleep(0.05)
    for pid in session_daemon_pids(socket_path):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 5
    while session_daemon_pids(socket_path) and time.monotonic() < deadline:
        time.sleep(0.05)


def test_the_session_daemon_of_an_interactive_run_uses_the_explicit_agent_env(daimonos_binary, tmp_path):
    """`agent --interactive` starts the session daemon as a child process. The
    daemon asks its provider for the model catalog at startup. That request
    must go to the endpoint of the `--agent-env` file, not to the endpoint of
    the home `agent.env` that the parent process loaded.
    """
    implicit, explicit = CatalogEndpoint(), CatalogEndpoint()
    try:
        _daemon_uses_the_explicit_file(daimonos_binary, tmp_path, implicit, explicit)
    finally:
        implicit.close()
        explicit.close()


def _daemon_uses_the_explicit_file(daimonos_binary, tmp_path, implicit, explicit):
    harness = Harness(daimonos_binary, tmp_path, explicit.port)
    write_home_agent_env(harness, implicit.port, model="test/model")
    socket_path = tmp_path / "daemon.sock"
    harness.config.write_text(
        "[analytics]\nenabled=false\n"
        f'[session]\nsocket_path = "{socket_path}"\nbootstrap_timeout_secs = 10\n'
        "bootstrap_retry_interval_ms = 10\n")
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
        os.close(master_fd)
        stop_session_daemons(socket_path)
    assert session_daemon_pids(socket_path) == [], "the test left a session daemon"
