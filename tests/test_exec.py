"""Tests for exec MCP tool."""

import json


def test_exec_captures_stdout(daimonos):
    result = daimonos.call_tool("exec", {
        "command": "echo",
        "args": ["hello world"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    assert content["out"] == "hello world"


def test_exec_captures_stderr_and_exit_code(daimonos):
    result = daimonos.call_tool("exec", {
        "command": "sh",
        "args": ["-c", "echo oops >&2; exit 7"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 7
    assert content["err"] == "oops"


def test_exec_with_env(daimonos):
    result = daimonos.call_tool("exec", {
        "command": "sh",
        "args": ["-c", "echo $MY_TEST_VAR"],
        "env": {"MY_TEST_VAR": "from_test"},
    })
    content = json.loads(result["content"][0]["text"])
    assert content["out"] == "from_test"


def test_exec_nonexistent_command(daimonos):
    result = daimonos.call_tool("exec", {
        "command": "/nonexistent_binary_xyz",
    })
    assert result.get("isError") is True


def test_exec_uses_workspace_cwd(daimonos):
    result = daimonos.call_tool("exec", {
        "command": "pwd",
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    assert content["out"] == daimonos.workspace


def test_exec_inherits_path_with_common_tool_dirs(daimonos):
    """Verify PATH includes auto-detected tool directories."""
    result = daimonos.call_tool("exec", {
        "command": "sh",
        "args": ["-c", "echo $PATH"],
    })
    content = json.loads(result["content"][0]["text"])
    path = content["out"]
    # PATH should contain the parent's PATH entries
    assert "/usr" in path, f"PATH should contain system dirs: {path}"


def test_exec_finds_cargo_directly(daimonos):
    """cargo should be found without sh -c wrapper if ~/.cargo/bin exists."""
    import os
    cargo_bin = os.path.expanduser("~/.cargo/bin/cargo")
    if not os.path.isfile(cargo_bin):
        import pytest
        pytest.skip("cargo not installed")

    result = daimonos.call_tool("exec", {
        "command": "cargo",
        "args": ["--version"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    assert "cargo" in content["out"]


def test_exec_large_output_is_capped(daimonos):
    """Very large stdout should be truncated with a notice."""
    result = daimonos.call_tool("exec", {
        "command": "sh",
        "args": ["-c", "seq 1 200000"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    out = content["out"]
    assert "truncated" in out
    assert "1\n" in out
    assert "200000" in out
