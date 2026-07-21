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
        response = json.loads(process.stdout.readline())
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
