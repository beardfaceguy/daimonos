import json
import os
import pty
import select
import signal
import socket
import subprocess
import time
from pathlib import Path


def test_normalized_mcp_subcommand_performs_handshake(daimonos_binary, tmp_path):
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(tmp_path), "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        request = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1.0.0"},
            },
        }
        process.stdin.write(json.dumps(request) + "\n")
        process.stdin.flush()
        response_line = process.stdout.readline()
        assert response_line, process.stderr.read()
        response = json.loads(response_line)
        assert response["id"] == 1
        assert response["result"]["serverInfo"]["name"] == "daimonos"
    finally:
        process.terminate()
        process.wait(timeout=5)


def test_help_lists_normalized_runtime_subcommands(daimonos_binary):
    completed = subprocess.run(
        [daimonos_binary, "--help"],
        capture_output=True,
        text=True,
        timeout=5,
        check=True,
    )
    assert "mcp " in completed.stdout
    assert "daemon " in completed.stdout
    assert "session-daemon " in completed.stdout
    assert "session " in completed.stdout


def test_agent_help_lists_explicit_interactive_and_print_modes(daimonos_binary):
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True,
        text=True,
        timeout=5,
        check=True,
    )

    assert "--interactive" in completed.stdout
    assert "--no-color" in completed.stdout
    assert "--print" in completed.stdout
    assert "--debug-thoughts" in completed.stdout
    assert "--debug-thoughts-path" in completed.stdout


def test_session_daemon_serves_local_attach_and_cleans_socket(
    daimonos_binary, tmp_path
):
    agent_env = tmp_path / "agent.env"
    agent_env.write_text(
        "\n".join(
            [
                "DAIMONOS_AGENT_PROVIDER=openrouter",
                "DAIMONOS_AGENT_MODEL=test/model",
                "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
                "DAIMONOS_AGENT_APPROVAL_MODE=auto",
                "DAIMONOS_AGENT_API_KEY=test",
                "DAIMONOS_AGENT_COMPACTION=off",
                "",
            ]
        )
    )
    socket_path = tmp_path / "session.sock"
    process = subprocess.Popen(
        [
            daimonos_binary,
            "--workspace",
            str(tmp_path),
            "session-daemon",
            "--socket",
            str(socket_path),
            "--agent-env",
            str(agent_env),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        deadline = time.monotonic() + 5
        while not socket_path.exists() and time.monotonic() < deadline:
            assert process.poll() is None, process.stderr.read()
            time.sleep(0.01)
        assert socket_path.exists()
        client.connect(str(socket_path))
        stream = client.makefile("rw")
        stream.write(
            json.dumps(
                {
                    "type": "attach",
                    "protocol_version": 2,
                    "client": {
                        "id": "pytest",
                        "kind": "headless",
                        "label": "pytest",
                    },
                    "requested_capabilities": ["observe"],
                }
            )
            + "\n"
        )
        stream.write(json.dumps({"type": "detach"}) + "\n")
        stream.flush()

        attached = json.loads(stream.readline())
        snapshot = json.loads(stream.readline())
        assert attached["type"] == "attach_ok"
        assert attached["session_id"]
        assert snapshot["type"] == "snapshot"
        assert snapshot["state"]["session_id"] == attached["session_id"]
    finally:
        client.close()
        process.terminate()
        process.wait(timeout=5)

    assert process.returncode == 0
    assert not socket_path.exists()


def test_interactive_without_tty_falls_back_before_loading_agent_env(
    daimonos_binary, tmp_path
):
    completed = subprocess.run(
        [
            daimonos_binary,
            "--workspace",
            str(tmp_path),
            "agent",
            "--interactive",
            "--provider",
            "openrouter",
            "--model",
            "test/model",
            "--agent-env",
            str(tmp_path / "missing.env"),
        ],
        capture_output=True,
        text=True,
        timeout=5,
    )

    assert completed.returncode != 0
    assert "agent task is required in print mode" in completed.stderr
    assert "--interactive was disabled because stdin or stdout is not a TTY" in completed.stderr
    assert "agent config" not in completed.stderr


def test_concurrent_interactive_ttys_share_bootstrapped_daemon_and_detach(
    daimonos_binary, tmp_path
):
    agent_env = tmp_path / "agent.env"
    agent_env.write_text(
        "\n".join(
            [
                "DAIMONOS_AGENT_PROVIDER=openrouter",
                "DAIMONOS_AGENT_MODEL=test/model",
                "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
                "DAIMONOS_AGENT_APPROVAL_MODE=auto",
                "DAIMONOS_AGENT_API_KEY=test",
                "DAIMONOS_AGENT_COMPACTION=off",
                "",
            ]
        )
    )
    socket_path = tmp_path / "bootstrapped.sock"
    config_path = tmp_path / "daimonos-bootstrap.toml"
    config_path.write_text(
        "\n".join(
            [
                "[session]",
                f'socket_path = "{socket_path}"',
                "bootstrap_timeout_secs = 5",
                "bootstrap_retry_interval_ms = 10",
                "client_command_timeout_secs = 30",
                "",
            ]
        )
    )
    processes = []
    for _ in range(2):
        master_fd, slave_fd = pty.openpty()
        process = subprocess.Popen(
            [
                daimonos_binary,
                "--workspace",
                str(tmp_path),
                "--config",
                str(config_path),
                "agent",
                "--interactive",
                "--provider",
                "openrouter",
                "--model",
                "test/model",
                "--agent-env",
                str(agent_env),
            ],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=subprocess.PIPE,
            text=True,
        )
        os.close(slave_fd)
        processes.append((process, master_fd))
    instance_path = tmp_path / "bootstrapped.sock.pid"
    daemon_pid = None
    try:
        deadline = time.monotonic() + 10
        while not instance_path.exists() and time.monotonic() < deadline:
            for process, _ in processes:
                assert process.poll() is None, process.stderr.read()
            time.sleep(0.01)
        assert instance_path.exists()
        daemon_pid = json.loads(instance_path.read_text())["pid"]
        daemon_status = (Path("/proc") / str(daemon_pid) / "status").read_text()
        daemon_ppid = int(
            next(
                line.split()[1]
                for line in daemon_status.splitlines()
                if line.startswith("PPid:")
            )
        )
        assert daemon_ppid not in {process.pid for process, _ in processes}

        terminal_output = {master_fd: b"" for _, master_fd in processes}
        pending = set(terminal_output)
        deadline = time.monotonic() + 15
        while pending and time.monotonic() < deadline:
            for process, _ in processes:
                assert process.poll() is None, process.stderr.read()
            readable, _, _ = select.select(list(pending), [], [], 0.05)
            for master_fd in readable:
                try:
                    terminal_output[master_fd] += os.read(master_fd, 65_536)
                except OSError as error:
                    if error.errno != 5:
                        raise
                    process = next(
                        process
                        for process, process_master in processes
                        if process_master == master_fd
                    )
                    process.wait(timeout=5)
                    raise AssertionError(process.stderr.read()) from error
                if b"\x1b[6n" in terminal_output[master_fd]:
                    os.write(master_fd, b"\x1b[1;1R")
                    pending.remove(master_fd)
        assert not pending
        time.sleep(0.05)
        for _, master_fd in processes:
            os.write(master_fd, b"/quit\r")
        for process, _ in processes:
            process.wait(timeout=10)
            stderr = process.stderr.read()
            assert process.returncode == 0, stderr
        assert socket_path.exists()
    finally:
        for process, _ in processes:
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
        for _, master_fd in processes:
            os.close(master_fd)

    assert not socket_path.exists()
    assert not instance_path.exists()


def test_interactive_bootstrap_failure_is_actionable(daimonos_binary, tmp_path):
    socket_path = tmp_path / "failed-bootstrap.sock"
    config_path = tmp_path / "failed-bootstrap.toml"
    config_path.write_text(
        "\n".join(
            [
                "[session]",
                f'socket_path = "{socket_path}"',
                "bootstrap_timeout_secs = 1",
                "bootstrap_retry_interval_ms = 10",
                "",
            ]
        )
    )
    master_fd, slave_fd = pty.openpty()
    process = subprocess.Popen(
        [
            daimonos_binary,
            "--workspace",
            str(tmp_path),
            "--config",
            str(config_path),
            "agent",
            "--interactive",
            "--agent-env",
            str(tmp_path / "missing.env"),
        ],
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=subprocess.PIPE,
        text=True,
    )
    os.close(slave_fd)
    try:
        process.wait(timeout=5)
        stderr = process.stderr.read()
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        os.close(master_fd)

    assert process.returncode != 0
    assert "timed out waiting for session daemon" in stderr
    assert "run `daimonos" in stderr
    assert "directly for startup diagnostics" in stderr
    assert not socket_path.exists()


def test_session_daemon_recovers_socket_after_sigkill(daimonos_binary, tmp_path):
    agent_env = tmp_path / "agent.env"
    agent_env.write_text(
        "\n".join(
            [
                "DAIMONOS_AGENT_PROVIDER=openrouter",
                "DAIMONOS_AGENT_MODEL=test/model",
                "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
                "DAIMONOS_AGENT_APPROVAL_MODE=auto",
                "DAIMONOS_AGENT_API_KEY=test",
                "DAIMONOS_AGENT_COMPACTION=off",
                "",
            ]
        )
    )
    socket_path = tmp_path / "stale.sock"
    instance_path = tmp_path / "stale.sock.pid"
    command = [
        daimonos_binary,
        "--workspace",
        str(tmp_path),
        "session-daemon",
        "--socket",
        str(socket_path),
        "--agent-env",
        str(agent_env),
    ]

    first = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    second = None
    try:
        deadline = time.monotonic() + 10
        while not instance_path.exists() and time.monotonic() < deadline:
            assert first.poll() is None
            time.sleep(0.01)
        first_pid = json.loads(instance_path.read_text())["pid"]
        first.kill()
        first.wait(timeout=5)
        assert socket_path.exists()

        second = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        second_pid = first_pid
        while second_pid == first_pid and time.monotonic() < deadline:
            assert second.poll() is None
            if instance_path.exists():
                second_pid = json.loads(instance_path.read_text())["pid"]
            time.sleep(0.01)
        assert second_pid == second.pid
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.connect(str(socket_path))
        client.close()
    finally:
        if first.poll() is None:
            first.kill()
            first.wait(timeout=5)
        if second is not None and second.poll() is None:
            second.terminate()
            second.wait(timeout=5)

    assert not socket_path.exists()
    assert not instance_path.exists()
