import json
import socket
import subprocess
import time


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
