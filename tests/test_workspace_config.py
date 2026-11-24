"""The agent frontends do not load `<workspace>/daimonos.toml` (ADR-039).

The file comes with the workspace. `agent`, `chat`, `acp`, and the session
daemon load it only when `--config` names it. The MCP server modes load it as
before.
"""
import json
import os
import signal
import subprocess
import time

from test_agent_oneshot_sessions import Harness, fake_provider, text_reply

__all__ = ["fake_provider"]  # the fixture of the imported module

NOTICE = "does not load a workspace daimonos.toml"


def servers_config(directory, marker):
    """Configuration text that names an MCP server whose command creates `marker`."""
    servers = directory / "servers.json"
    servers.write_text(json.dumps({"mcpServers": {"probe": {"command": "/usr/bin/touch", "args": [str(marker)]}}}))
    return "[analytics]\nenabled=false\n[agent.mcp]\nservers_file = " + json.dumps(str(servers)) + "\n"


def run(harness, *argv, stdin=subprocess.DEVNULL):
    return subprocess.run([harness.binary, "--workspace", str(harness.workspace), *argv],
                          cwd=harness.workspace, env=harness.environment(),
                          stdin=stdin, capture_output=True, text=True, timeout=60)


def loaded_from(stderr):
    return [line for line in stderr.splitlines() if line.startswith("config: loaded from")]


def test_agent_does_not_load_the_workspace_config(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("done")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    marker = tmp_path / "MARKER"
    (harness.workspace / "daimonos.toml").write_text(servers_config(harness.workspace, marker))

    done = run(harness, "agent", "--agent-env", str(harness.agent_env), "task")
    assert done.returncode == 0, done.stderr
    assert done.stdout == "done\n"
    assert not marker.exists(), "a command that the workspace configuration names ran"
    assert loaded_from(done.stderr) == []
    assert NOTICE in done.stderr and "--config" in done.stderr


def test_the_workspace_config_applies_when_the_config_option_names_it(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([text_reply("done")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    marker = tmp_path / "MARKER"
    workspace_config = harness.workspace / "daimonos.toml"
    workspace_config.write_text(servers_config(harness.workspace, marker))

    done = run(harness, "--config", str(workspace_config), "agent", "--agent-env", str(harness.agent_env), "task")
    assert done.returncode == 0, done.stderr
    assert marker.exists(), "the named configuration did not apply"
    assert NOTICE not in done.stderr


def test_the_user_config_applies_when_a_workspace_config_exists(daimonos_binary, tmp_path, fake_provider):
    # Before ADR-039 the workspace file was found first and the user file was not read.
    provider = fake_provider([text_reply("done")])
    harness = Harness(daimonos_binary, tmp_path, provider.port)
    workspace_marker, user_marker = tmp_path / "WORKSPACE_MARKER", tmp_path / "USER_MARKER"
    (harness.workspace / "daimonos.toml").write_text(servers_config(harness.workspace, workspace_marker))
    user_directory = harness.home / ".config" / "daimonos"
    user_directory.mkdir(parents=True)
    (user_directory / "config.toml").write_text(servers_config(user_directory, user_marker))

    done = run(harness, "agent", "--agent-env", str(harness.agent_env), "task")
    assert done.returncode == 0, done.stderr
    assert user_marker.exists() and not workspace_marker.exists()
    assert len(loaded_from(done.stderr)) == 1 and str(user_directory / "config.toml") in loaded_from(done.stderr)[0]


# A file that the startup validation rejects. A mode that loads it stops with a
# configuration error. It starts nothing and makes no request.
INVALID = '[observability]\nenabled = true\nendpoint = "file:///not-http"\n'


def test_the_rule_applies_with_a_state_dir_and_under_confinement(daimonos_binary, tmp_path, fake_provider):
    # The run succeeds, so the workspace file was not loaded. The control shows
    # that the same file stops the run when it is named.
    for index, extra in enumerate([(), ("--confine-paths",)]):
        provider = fake_provider([text_reply("done"), text_reply("done")])
        harness = Harness(daimonos_binary, tmp_path / str(index), provider.port)
        workspace_config = harness.workspace / "daimonos.toml"
        workspace_config.write_text(INVALID)
        state = tmp_path / str(index) / "state"
        state.mkdir(mode=0o700)
        common = ("--state-dir", str(state), *extra)
        agent = ("agent", "--agent-env", str(harness.agent_env), "task")

        done = run(harness, *common, *agent)
        assert done.returncode == 0, (extra, done.stderr)
        assert done.stdout == "done\n", extra
        assert loaded_from(done.stderr) == [], extra
        assert NOTICE in done.stderr, extra

        named = run(harness, *common, "--config", str(workspace_config), *agent)
        assert named.returncode != 0, (extra, "the named file did not apply")
        assert "observability.endpoint" in named.stderr, (extra, named.stderr)
        assert len(provider.requests) == 1, extra


def test_acp_and_chat_do_not_load_the_workspace_config(daimonos_binary, tmp_path):
    for index, mode in enumerate(["acp", "chat"]):
        harness = Harness(daimonos_binary, tmp_path / str(index), 1)
        workspace_config = harness.workspace / "daimonos.toml"
        workspace_config.write_text(INVALID)
        # Standard input is at its end, so the frontend stops after startup.
        done = run(harness, mode, "--agent-env", str(harness.agent_env))
        assert done.returncode == 0, (mode, done.stderr)
        assert loaded_from(done.stderr) == [], mode
        assert NOTICE in done.stderr, mode

        named = run(harness, "--config", str(workspace_config), mode, "--agent-env", str(harness.agent_env))
        assert named.returncode != 0, (mode, "the named file did not apply")
        assert "observability.endpoint" in named.stderr, (mode, named.stderr)


def test_the_session_daemon_does_not_load_the_workspace_config(daimonos_binary, tmp_path):
    """Start the session daemon directly. A daemon that loads the workspace
    file stops with a configuration error. A daemon that does not load it
    starts and creates its socket.
    """
    harness = Harness(daimonos_binary, tmp_path, 1)
    workspace_config = harness.workspace / "daimonos.toml"
    workspace_config.write_text(INVALID)
    socket_path = tmp_path / "daemon.sock"
    daemon = ("session-daemon", "--socket", str(socket_path), "--agent-env", str(harness.agent_env))

    named = run(harness, "--config", str(workspace_config), *daemon)
    assert named.returncode != 0, "the named file did not apply"
    assert "observability.endpoint" in named.stderr, named.stderr
    assert not socket_path.exists()

    process = subprocess.Popen(
        [harness.binary, "--workspace", str(harness.workspace), *daemon], cwd=harness.workspace,
        env=harness.environment(), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.monotonic() + 20
        while not socket_path.exists() and time.monotonic() < deadline:
            assert process.poll() is None, process.stderr.read()
            time.sleep(0.02)
        assert socket_path.exists(), "the session daemon did not start"
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.kill(process.pid, signal.SIGKILL)
                process.wait()
    stderr = process.stderr.read()
    assert loaded_from(stderr) == []
    assert NOTICE in stderr


def test_the_mcp_server_loads_the_workspace_config_as_before(daimonos_binary, tmp_path):
    harness = Harness(daimonos_binary, tmp_path, 1)
    workspace_config = harness.workspace / "daimonos.toml"
    workspace_config.write_text("[analytics]\nenabled=false\n")

    done = run(harness, "--mcp", "--verbose")
    assert len(loaded_from(done.stderr)) == 1 and str(workspace_config) in loaded_from(done.stderr)[0]
    assert NOTICE not in done.stderr
