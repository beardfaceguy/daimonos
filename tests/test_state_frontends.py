import hashlib
import json
import os
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import pytest


def test_session_daemon_uses_selected_state_root(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    env.pop("RUSTUP_HOME", None)
    env.pop("CARGO_HOME", None)
    with tempfile.TemporaryDirectory(prefix="ds-") as state_name:
        state = Path(state_name)
        workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()[:16]
        socket_path = state / "sockets" / f"session-{workspace_id}.sock"
        process = subprocess.Popen(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "session-daemon", "--agent-env", str(agent_env)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        try:
            deadline = time.monotonic() + 10
            client = None
            while time.monotonic() < deadline:
                assert process.poll() is None, process.stderr.read()
                candidate = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                try:
                    candidate.connect(str(socket_path))
                except (FileNotFoundError, ConnectionRefusedError):
                    candidate.close()
                    time.sleep(0.01)
                else:
                    client = candidate
                    break
            assert client is not None, "session daemon did not listen on the selected state socket"
            with client, client.makefile("rw") as stream:
                stream.write(json.dumps({
                    "type": "attach", "protocol_version": 3,
                    "client": {"id": "pytest", "kind": "headless", "label": "pytest"},
                    "requested_capabilities": ["observe"],
                }) + "\n")
                stream.write(json.dumps({"type": "detach"}) + "\n")
                stream.flush()
                assert json.loads(stream.readline())["type"] == "attach_ok"
                assert json.loads(stream.readline())["type"] == "snapshot"
            assert (state / "sessions" / "daemon" / "sessions.sqlite3").is_file()
            assert (state / "catalog" / "catalog.sqlite").is_file()
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=5)
        assert process.returncode == 0
        assert list(home.iterdir()) == []
        assert list(workspace.iterdir()) == []


def test_session_daemon_rejects_socket_outside_state(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside.sock"
    agent_env = tmp_path / "agent.env"
    agent_env.write_text(
        "\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter",
            "DAIMONOS_AGENT_MODEL=test/model",
            "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_COMPACTION=off",
            "",
        ])
    )
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session-daemon", "--agent-env", str(agent_env), "--socket", str(outside)],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env,
    )
    try:
        try:
            _, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            outside_created = outside.exists()
            process.terminate()
            _, stderr = process.communicate(timeout=5)
            pytest.fail(f"daemon accepted an outside socket: {outside_created}; {stderr}")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)

    assert process.returncode == 2, stderr
    error = json.loads(stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert not outside.exists()
    assert not (state / "logs").exists()
    assert list(home.iterdir()) == []


@pytest.mark.parametrize("agent_env_value", [None, "", "   "])
def test_session_daemon_does_not_discover_home_agent_env(daimonos_binary, tmp_path, agent_env_value):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    config = home / ".config" / "daimonos"
    config.mkdir(parents=True)
    agent_env = config / "agent.env"
    original = "\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ])
    agent_env.write_text(original)
    with tempfile.TemporaryDirectory(prefix="ds-") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
        if agent_env_value is not None:
            env["DAIMONOS_AGENT_ENV"] = agent_env_value
        process = subprocess.Popen(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "session-daemon"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, env=env,
        )
        try:
            try:
                _, stderr = process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                process.terminate()
                _, stderr = process.communicate(timeout=5)
                pytest.fail(f"daemon used implicit HOME agent configuration: {stderr}")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)

    assert process.returncode != 0
    assert "--agent-env" in stderr or "DAIMONOS_AGENT_ENV" in stderr
    assert agent_env.read_text() == original
    assert not (home / ".daimonos").exists()
    assert list(workspace.iterdir()) == []


def test_session_daemon_rejects_approvals_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    evidence = workspace / "evidence.txt"
    evidence.write_text("unchanged\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    approvals = state / "sessions" / "approvals"
    approvals.mkdir(parents=True)
    (approvals / "agent-approvals").symlink_to(evidence)
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session-daemon", "--agent-env", str(tmp_path / "unused.env")],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert evidence.read_text() == "unchanged\n"
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert not (state / "logs").exists()


def test_session_daemon_fails_closed_on_corrupt_state_catalog(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    catalog = state / "catalog"
    catalog.mkdir()
    (catalog / "catalog.sqlite").write_bytes(b"not a sqlite database")
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session-daemon", "--agent-env", str(agent_env), "--socket", str(state / "s.sock")],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env,
    )
    try:
        try:
            _, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            process.terminate()
            _, stderr = process.communicate(timeout=5)
            pytest.fail(f"daemon continued with an unusable state catalog: {stderr}")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)

    assert process.returncode != 0
    assert "catalog" in stderr.lower()
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_session_daemons_use_distinct_sockets_for_workspaces(daimonos_binary, tmp_path):
    workspaces = [tmp_path / "one", tmp_path / "two"]
    for workspace in workspaces:
        workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    with tempfile.TemporaryDirectory(prefix="ds-") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
        processes = []
        try:
            for workspace in workspaces:
                processes.append(subprocess.Popen(
                    [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
                     "session-daemon", "--agent-env", str(agent_env)],
                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    text=True, env=env,
                ))
            sockets = [state / "sockets" / (
                "session-" + hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()[:16] + ".sock"
            ) for workspace in workspaces]
            assert sockets[0] != sockets[1]
            for process, socket_path in zip(processes, sockets):
                deadline = time.monotonic() + 10
                while True:
                    assert process.poll() is None, process.stderr.read()
                    try:
                        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                            client.connect(str(socket_path))
                        break
                    except (FileNotFoundError, ConnectionRefusedError):
                        if time.monotonic() >= deadline:
                            pytest.fail(f"socket not ready: {socket_path}")
                        time.sleep(0.01)
            assert all(process.poll() is None for process in processes)
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
            for process in processes:
                process.communicate(timeout=5)
    assert list(home.iterdir()) == []
    assert all(list(workspace.iterdir()) == [] for workspace in workspaces)


@pytest.mark.parametrize("skill_source", ["default", "env"])
def test_acp_uses_selected_state_and_ignores_home_skills(daimonos_binary, tmp_path, skill_source):
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    skill = home / ".agents" / "skills" / "host-only"
    skill.mkdir(parents=True)
    skill_file = skill / "SKILL.md"
    skill_file.write_text("---\nname: host-only\ndescription: Host-only skill\n---\nHost content\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
    ] + ([f"DAIMONOS_SKILL_DIR={skill.parent}"] if skill_source == "env" else []) + [""]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
    command += ["acp", "--agent-env", str(agent_env)]
    process = subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        init = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "result" in init, init
        created = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
            "cwd": str(workspace), "mcpServers": [],
        }})
        assert "sessionId" in created.get("result", {}), created
        listed = client.send_raw({"jsonrpc": "2.0", "id": 3, "method": "session/list", "params": {}})
        assert "result" in listed, listed
        for request_id in range(4, 14):
            notifications = json.dumps(client.notifications)
            if '"clear"' in notifications and '"usage"' in notifications:
                break
            client.send_raw({"jsonrpc": "2.0", "id": request_id,
                             "method": "session/list", "params": {}})
        notifications = json.dumps(client.notifications)
        assert '"clear"' in notifications and '"usage"' in notifications
        assert ("host-only" in notifications) is (skill_source != "default")
        assert (state / "sessions" / "acp" / "sessions.sqlite3").is_file()
        session_id = created["result"]["sessionId"]
    finally:
        if process.poll() is None:
            _terminate(process)

    restored = subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    try:
        client = DaimonosClient(restored, str(workspace))
        initialized = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "result" in initialized, initialized
        loaded = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {
            "sessionId": session_id, "cwd": str(workspace), "mcpServers": [],
        }})
        assert "result" in loaded, loaded
    finally:
        if restored.poll() is None:
            _terminate(restored)

    assert skill_file.read_text().endswith("Host content\n")
    assert not (home / ".daimonos").exists()
    assert not (home / ".config").exists()
    assert list(workspace.iterdir()) == []


def test_session_daemon_fails_closed_on_corrupt_state_analytics(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    analytics = state / "analytics"
    analytics.mkdir()
    (analytics / "analytics.db").write_bytes(b"not a sqlite database")
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session-daemon", "--agent-env", str(agent_env), "--socket", str(state / "s.sock")],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env,
    )
    try:
        try:
            _, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            process.terminate()
            _, stderr = process.communicate(timeout=5)
            pytest.fail(f"daemon continued with unusable state analytics: {stderr}")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)

    assert process.returncode != 0
    assert "analytics" in stderr.lower()
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_acp_fails_closed_on_corrupt_state_analytics(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    analytics = state / "analytics"
    analytics.mkdir()
    (analytics / "analytics.db").write_bytes(b"not a sqlite database")
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "acp", "--agent-env", str(agent_env)],
        input="", capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode != 0
    assert "analytics" in completed.stderr.lower()
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_acp_rejects_trace_directory_outside_selected_state(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path),
               DAIMONOS_OPENROUTER_TRACE_DIR=str(workspace))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "acp", "--agent-env", str(tmp_path / "unused.env")],
        input="", capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "DAIMONOS_OPENROUTER_TRACE_DIR" in error["message"]
    assert not (state / "logs").exists()
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_chat_list_uses_selected_state_store(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    old_store = home / ".daimonos" / "chat-sessions"
    old_store.mkdir(parents=True)
    marker = old_store / "host-only.txt"
    marker.write_text("unchanged")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "chat", "--list"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 0, completed.stderr
    assert (state / "sessions" / "chat" / "sessions.sqlite3").is_file()
    assert list(old_store.iterdir()) == [marker]
    assert marker.read_text() == "unchanged"
    assert not (home / "config").exists()
    assert list(workspace.iterdir()) == []


@pytest.mark.parametrize("explicit_config", [False, True])
def test_session_daemon_ignores_implicit_home_mcp_servers(daimonos_binary, tmp_path, explicit_config):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    zed = home / ".config" / "zed"
    zed.mkdir(parents=True)
    marker = tmp_path / "unexpected-mcp-start"
    settings = zed / "settings.json"
    settings.write_text(json.dumps({"context_servers": {
        "probe": {"command": "/usr/bin/touch", "args": [str(marker)]},
    }}))
    config = tmp_path / "explicit.toml"
    config.write_text("[agent.mcp]\nservers_file = " + json.dumps(str(settings)) + "\n")
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    with tempfile.TemporaryDirectory(prefix="ds-") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()[:16]
        socket_path = state / "sockets" / f"session-{workspace_id}.sock"
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
        command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
        if explicit_config:
            command += ["--config", str(config)]
        process = subprocess.Popen(
            command + ["session-daemon", "--agent-env", str(agent_env)],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, env=env,
        )
        try:
            deadline = time.monotonic() + 10
            while True:
                assert process.poll() is None, process.stderr.read()
                try:
                    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    client.connect(str(socket_path))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    client.close()
                    if time.monotonic() >= deadline:
                        pytest.fail("daemon socket did not become ready")
                    time.sleep(0.01)
            with client, client.makefile("rwb") as stream:
                request = {"type": "attach", "protocol_version": 3,
                           "client": {"id": "pytest", "kind": "headless", "label": "pytest"},
                           "requested_capabilities": ["observe"]}
                stream.write((json.dumps(request) + "\n").encode())
                stream.flush()
                line = stream.readline()
                if not line:
                    if process.poll() is None:
                        process.terminate()
                    _, stderr = process.communicate(timeout=5)
                    pytest.fail(f"daemon closed the attach connection: {stderr}")
                response = json.loads(line)
                assert response["type"] == "attach_ok", response
            assert marker.exists() is explicit_config, "daemon selected the wrong MCP configuration"
        finally:
            if process.poll() is None:
                process.terminate()
            process.communicate(timeout=5)
    assert list(workspace.iterdir()) == []


def test_agent_dry_run_uses_selected_state(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "agent", "--dry-run", "inspect the workspace"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 0, completed.stderr
    assert "inspect the workspace" in completed.stdout
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_acp_rejects_hardlinked_token_debug_file(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    evidence = workspace / "evidence.txt"
    evidence.write_text("unchanged")
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    logs = state / "logs"
    logs.mkdir()
    os.link(evidence, logs / "token-debug.log")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--debug-tokens", "acp", "--agent-env", str(tmp_path / "unused.env")],
        input="", capture_output=True, text=True, timeout=10, env=env,
    )
    assert evidence.read_text() == "unchanged"
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert not (logs / "daimonos").exists()


def test_agent_rejects_thought_log_outside_selected_state(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    evidence = workspace / "evidence.txt"
    evidence.write_text("unchanged")
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "agent", "--debug-thoughts", "--debug-thoughts-path", str(evidence),
         "--agent-env", str(tmp_path / "unused.env"), "inspect"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert evidence.read_text() == "unchanged"
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "--debug-thoughts-path" in error["message"]
    assert not (state / "logs").exists()
    assert list(home.iterdir()) == []


@pytest.mark.parametrize("custom_logs", [False, True])
def test_agent_print_routes_debug_files_to_selected_state(daimonos_binary, tmp_path, custom_logs):
    from acp_crash_probe import BASELINE, MockProvider

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    log_dir = state / ("custom-logs" if custom_logs else "logs")
    evidence = workspace / "evidence.txt"
    config = tmp_path / "logging.toml"
    if custom_logs:
        evidence.write_text("unchanged")
        (state / "logs").mkdir()
        (state / "logs" / "token-debug.log").symlink_to(evidence)
        config.write_text("[logging]\ndirectory = " + json.dumps(str(log_dir)) + "\n")
    provider = MockProvider()
    try:
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter",
            "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.server.server_port}/v1",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_COMPACTION=off",
            "DAIMONOS_AGENT_MODELS_DISCOVER=off",
            "",
        ]))
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
        command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
        if custom_logs:
            command += ["--config", str(config)]
        completed = subprocess.run(
            command + ["--debug-tokens", "agent", "--print", "--debug-thoughts",
                       "--agent-env", str(agent_env), "hello"],
            capture_output=True, text=True, timeout=30, env=env,
        )
    finally:
        provider.close()
    assert completed.returncode == 0, completed.stderr
    assert BASELINE in completed.stdout
    assert (log_dir / "thought-debug.log").is_file()
    assert (log_dir / "token-debug.log").is_file()
    assert (state / "analytics" / "analytics.db").is_file()
    assert list(home.iterdir()) == []
    if custom_logs:
        assert evidence.read_text() == "unchanged"
        assert list(workspace.iterdir()) == [evidence]
    else:
        assert list(workspace.iterdir()) == []


def test_interactive_agent_bootstraps_daemon_with_selected_state(daimonos_binary, tmp_path):
    import pty
    import select
    import signal

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "",
    ]))
    instructions = tmp_path / "explicit-instructions.md"
    instructions.write_text("explicit agent instructions")
    with tempfile.TemporaryDirectory(prefix="ds-") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()[:16]
        socket_path = state / "sockets" / f"session-{workspace_id}.sock"
        instance_path = socket_path.with_suffix(".sock.pid")
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
        master_fd, slave_fd = pty.openpty()
        process = subprocess.Popen(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "--agent-instructions", str(instructions), "--debug-tokens",
             "agent", "--interactive", "--agent-env", str(agent_env)],
            stdin=slave_fd, stdout=slave_fd, stderr=subprocess.PIPE, text=True, env=env,
        )
        os.close(slave_fd)
        daemon_pid = None
        try:
            deadline = time.monotonic() + 15
            while not instance_path.exists() and time.monotonic() < deadline:
                assert process.poll() is None, process.stderr.read()
                time.sleep(0.01)
            assert instance_path.exists(), "agent did not start a selected-state daemon"
            daemon_pid = json.loads(instance_path.read_text())["pid"]
            cmdline = (Path("/proc") / str(daemon_pid) / "cmdline").read_bytes().split(b"\0")
            assert b"--state-dir" in cmdline
            assert os.fsencode(state.resolve()) in cmdline
            assert b"--agent-instructions" in cmdline
            assert os.fsencode(instructions) in cmdline
            assert b"--debug-tokens" in cmdline
            output = b""
            deadline = time.monotonic() + 15
            while b"\x1b[6n" not in output and time.monotonic() < deadline:
                assert process.poll() is None, process.stderr.read()
                readable, _, _ = select.select([master_fd], [], [], 0.05)
                if readable:
                    output += os.read(master_fd, 65536)
            assert b"\x1b[6n" in output
            os.write(master_fd, b"\x1b[1;1R")
            time.sleep(0.05)
            os.write(master_fd, b"/quit\r")
            process.wait(timeout=10)
            assert process.returncode == 0, process.stderr.read()
            assert (state / "sessions" / "daemon" / "sessions.sqlite3").is_file()
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            if daemon_pid is not None:
                try:
                    os.kill(daemon_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                deadline = time.monotonic() + 5
                while socket_path.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
            os.close(master_fd)
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_mcp_auth_status_uses_selected_credentials(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    legacy = home / ".blue_rose" / "mcp-oauth"
    legacy.mkdir(parents=True, mode=0o700)
    (home / ".blue_rose").chmod(0o700)
    endpoint = "https://example.com/mcp"
    key = hashlib.sha256((endpoint + "\0default").encode()).hexdigest()
    credential = legacy / (key + ".json")
    credential.write_text(json.dumps({"client_id": "test", "access_token": "test-token"}))
    credential.chmod(0o600)
    original = credential.read_bytes()
    config = tmp_path / "config.toml"
    config.write_text('[acp.mcp.oauth_servers.demo]\nurl = "https://example.com/mcp"\n')
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    prefix = [daimonos_binary, "--workspace", str(workspace), "--config", str(config)]
    legacy_status = subprocess.run(prefix + ["mcp", "auth", "status", "demo"],
                                   capture_output=True, text=True, timeout=10, env=env)
    assert legacy_status.returncode == 0, legacy_status.stderr
    assert "authenticated" in legacy_status.stdout
    selected_status = subprocess.run(prefix + ["--state-dir", str(state), "mcp", "auth", "status", "demo"],
                                     capture_output=True, text=True, timeout=10, env=env)
    assert selected_status.returncode == 0, selected_status.stderr
    assert "auth_required" in selected_status.stdout
    (state / "oauth").mkdir(mode=0o700)
    os.link(credential, state / "oauth" / (key + ".json"))
    hardlinked_status = subprocess.run(prefix + ["--state-dir", str(state), "mcp", "auth", "status", "demo"],
                                       capture_output=True, text=True, timeout=10, env=env)
    assert hardlinked_status.returncode != 0, hardlinked_status.stdout
    assert "authenticated" not in hardlinked_status.stdout
    assert credential.read_bytes() == original
    assert list(workspace.iterdir()) == []


@pytest.mark.parametrize("explicit_settings", [False, True])
def test_acp_zed_fallback_requires_explicit_settings_with_state(daimonos_binary, tmp_path, explicit_settings):
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    zed = home / ".config" / "zed"
    zed.mkdir(parents=True)
    marker = tmp_path / "zed-server-started"
    settings = zed / "settings.json"
    settings.write_text(json.dumps({"context_servers": {
        "probe": {"command": "/usr/bin/touch", "args": [str(marker)]},
    }}))
    config = tmp_path / "explicit.toml"
    config.write_text("[acp.mcp]\nzed_settings_path = " + json.dumps(str(settings)) + "\n")
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        "",
    ]))
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
    if explicit_settings:
        command += ["--config", str(config)]
    process = subprocess.Popen(
        command + ["acp", "--agent-env", str(agent_env)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        initialized = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "Zed", "version": "1"},
        }})
        assert "result" in initialized, initialized
        created = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
            "cwd": str(workspace), "mcpServers": [],
        }})
        assert "sessionId" in created.get("result", {}), created
        assert marker.exists() is explicit_settings
    finally:
        if process.poll() is None:
            _terminate(process)
    assert list(workspace.iterdir()) == []


def test_stats_reads_selected_state_not_home(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    old_analytics = home / ".daimonos" / "analytics.db"
    old_analytics.parent.mkdir(parents=True)
    old_analytics.write_bytes(b"host-data")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state), "--stats"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 0, completed.stderr
    assert "No analytics data found" in completed.stderr
    assert str(state / "analytics" / "analytics.db") in completed.stderr
    assert old_analytics.read_bytes() == b"host-data"
    assert list(workspace.iterdir()) == []


def test_session_daemon_rejects_hardlinked_socket_lock(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    evidence = workspace / "evidence.txt"
    evidence.write_text("unchanged")
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    socket_path = state / "s.sock"
    os.link(evidence, state / "s.sock.lock")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session-daemon", "--agent-env", str(tmp_path / "unused.env"),
         "--socket", str(socket_path)],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env,
    )
    try:
        _, stderr = process.communicate(timeout=10)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
    assert evidence.read_text() == "unchanged"
    assert process.returncode == 2, stderr
    error = json.loads(stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert not (state / "logs").exists()


def test_chat_persists_selected_state_session(daimonos_binary, tmp_path):
    import pty
    import select
    import sqlite3
    from acp_crash_probe import BASELINE, MockProvider

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    provider = MockProvider()
    master_fd, slave_fd = pty.openpty()
    process = None
    try:
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter",
            "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.server.server_port}/v1",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_COMPACTION=off",
            "DAIMONOS_AGENT_MODELS_DISCOVER=off",
            "",
        ]))
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path), TERM="xterm")
        process = subprocess.Popen(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "chat", "--agent-env", str(agent_env)],
            stdin=slave_fd, stdout=slave_fd, stderr=subprocess.PIPE, env=env,
        )
        os.close(slave_fd)
        slave_fd = None
        output = b""
        deadline = time.monotonic() + 15
        sent_prompt = False
        while BASELINE.encode() not in output and time.monotonic() < deadline:
            assert process.poll() is None, process.stderr.read().decode(errors="replace")
            readable, _, _ = select.select([master_fd], [], [], 0.05)
            if readable:
                chunk = os.read(master_fd, 65536)
                output += chunk
                if b"\x1b[6n" in chunk:
                    os.write(master_fd, b"\x1b[1;1R")
            if not sent_prompt and b"daimonos chat" in output:
                os.write(master_fd, b"hello\r")
                sent_prompt = True
        assert BASELINE.encode() in output, output.decode(errors="replace")
        os.write(master_fd, b"/quit\r")
        process.wait(timeout=10)
        assert process.returncode == 0, process.stderr.read().decode(errors="replace")
        database = state / "sessions" / "chat" / "sessions.sqlite3"
        with sqlite3.connect(database) as connection:
            payload = connection.execute("SELECT payload FROM sessions").fetchone()[0]
        assert BASELINE.encode() in payload
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        if slave_fd is not None:
            os.close(slave_fd)
        os.close(master_fd)
        provider.close()
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


@pytest.mark.parametrize("explicit_instructions", [False, True])
def test_agent_state_ignores_implicit_home_instructions(daimonos_binary, tmp_path, explicit_instructions):
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    captured = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            payload = json.dumps({"data": [{"id": "test/model", "context_length": 100000,
                                           "supported_parameters": ["tools"]}]}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_POST(self):
            captured.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            chunk = {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"},
                                  "finish_reason": "stop"}]}
            payload = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        (workspace / "agent-instructions.md").write_text("WORKSPACE_INSTRUCTION_MARKER")
        home = tmp_path / "home"
        config_home = home / ".config" / "daimonos"
        config_home.mkdir(parents=True)
        host_instructions = config_home / "agent-instructions.md"
        host_instructions.write_text("HOME_INSTRUCTION_MARKER")
        state = tmp_path / "state"
        state.mkdir(mode=0o700)
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter",
            "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{server.server_port}/v1",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_COMPACTION=off",
            "DAIMONOS_AGENT_MODELS_DISCOVER=off",
            "",
        ]))
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                   XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
        command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
        if explicit_instructions:
            command += ["--agent-instructions", str(host_instructions)]
        completed = subprocess.run(
            command + ["agent", "--print", "--agent-env", str(agent_env), "hello"],
            capture_output=True, text=True, timeout=30, env=env,
        )
        assert completed.returncode == 0, completed.stderr
        sent = json.dumps(captured)
        assert ("HOME_INSTRUCTION_MARKER" in sent) is explicit_instructions
        assert ("WORKSPACE_INSTRUCTION_MARKER" in sent) is not explicit_instructions
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def test_prompt_dump_requires_explicit_output_with_selected_state(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state), "--dump-prompts"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "--dump-prompts" in error["message"]
    output = tmp_path / "explicit-prompts"
    explicit = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--dump-prompts", str(output)],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert explicit.returncode == 0, explicit.stderr
    assert (output / "tool_descriptions.toml").is_file()
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_agent_state_preserves_thought_log_flag_validation(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "agent", "--dry-run", "--debug-thoughts-path", str(state / "thoughts.log"), "inspect"],
        capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode != 0
    assert "required arguments" in completed.stderr
    assert "--debug-thoughts" in completed.stderr
    assert not (state / "thoughts.log").exists()
