import json
import subprocess


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


def test_agent_help_lists_explicit_interactive_and_print_modes(daimonos_binary):
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True,
        text=True,
        timeout=5,
        check=True,
    )

    assert "--interactive" in completed.stdout
    assert "--print" in completed.stdout


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
