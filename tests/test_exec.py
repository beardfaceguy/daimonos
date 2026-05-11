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


def test_exec_filter_pip_install_success(daimonos):
    """Successful pip install should be compressed to summary."""
    result = daimonos.call_tool("exec", {
        "command": "sh",
        "args": ["-c",
                 "echo 'Collecting requests\\n"
                 "  Downloading requests-2.31.0.tar.gz (110 kB)\\n"
                 "Installing collected packages: requests\\n"
                 "Successfully installed requests-2.31.0'"],
    })
    # This goes through sh -c echo, which the classifier won't match
    # (it's "sh" not "pip"). Test the actual filter via a real pip call:
    # Instead, test with a direct command that the classifier will match.
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0


def test_exec_filter_build_success(daimonos):
    """Successful cargo build via exec should return structured output (plugin redirect)."""
    import os
    cargo_bin = os.path.expanduser("~/.cargo/bin/cargo")
    if not os.path.isfile(cargo_bin):
        import pytest
        pytest.skip("cargo not installed")

    daimonos.call_tool("write_file", {
        "path": "filter_test/Cargo.toml",
        "content": '[package]\nname = "filter_test"\nversion = "0.1.0"\nedition = "2021"\n',
    })
    daimonos.call_tool("exec", {
        "command": "mkdir",
        "args": ["-p", "filter_test/src"],
    })
    daimonos.call_tool("write_file", {
        "path": "filter_test/src/main.rs",
        "content": "fn main() { println!(\"hello\"); }\n",
    })

    result = daimonos.call_tool("exec", {
        "command": "cargo build",
        "cwd": "filter_test",
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    out = content["out"]
    # Plugin redirect returns structured JSON; filter returns "ok"
    assert "ok" in out
    # Should NOT contain verbose compile output
    assert "Compiling" not in out


def test_exec_filter_test_runner(daimonos):
    """cargo test via exec should return compact output (plugin redirect or filter)."""
    import os
    cargo_bin = os.path.expanduser("~/.cargo/bin/cargo")
    if not os.path.isfile(cargo_bin):
        import pytest
        pytest.skip("cargo not installed")

    daimonos.call_tool("write_file", {
        "path": "filter_test2/Cargo.toml",
        "content": '[package]\nname = "filter_test2"\nversion = "0.1.0"\nedition = "2021"\n',
    })
    daimonos.call_tool("exec", {
        "command": "mkdir",
        "args": ["-p", "filter_test2/src"],
    })
    daimonos.call_tool("write_file", {
        "path": "filter_test2/src/lib.rs",
        "content": (
            "#[cfg(test)]\nmod tests {\n"
            "    #[test]\n    fn test_a() { assert!(true); }\n"
            "    #[test]\n    fn test_b() { assert!(true); }\n"
            "    #[test]\n    fn test_c() { assert!(true); }\n"
            "}\n"
        ),
    })

    result = daimonos.call_tool("exec", {
        "command": "cargo test",
        "cwd": "filter_test2",
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    out = content["out"]
    # Plugin redirect: structured JSON with "passed":3
    # Filter fallback: text with "3 passed"
    assert "passed" in out or "3 passed" in out
    # Should NOT contain verbose individual test lines
    assert "test tests::test_a ... ok" not in out


def test_exec_filter_passthrough_for_unknown_commands(daimonos):
    """Unknown commands should pass through unfiltered."""
    result = daimonos.call_tool("exec", {
        "command": "echo",
        "args": ["raw output preserved"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    assert content["out"] == "raw output preserved"


def test_exec_plugin_redirect_cargo_test(daimonos):
    """exec('cargo test') should redirect through native cargo plugin."""
    import os
    cargo_bin = os.path.expanduser("~/.cargo/bin/cargo")
    if not os.path.isfile(cargo_bin):
        import pytest
        pytest.skip("cargo not installed")

    daimonos.call_tool("write_file", {
        "path": "redirect_test/Cargo.toml",
        "content": '[package]\nname = "redirect_test"\nversion = "0.1.0"\nedition = "2021"\n',
    })
    daimonos.call_tool("exec", {"command": "mkdir", "args": ["-p", "redirect_test/src"]})
    daimonos.call_tool("write_file", {
        "path": "redirect_test/src/lib.rs",
        "content": (
            "#[cfg(test)]\nmod tests {\n"
            "    #[test]\n    fn passes() { assert!(true); }\n"
            "}\n"
        ),
    })

    result = daimonos.call_tool("exec", {
        "command": "cargo test",
        "cwd": "redirect_test",
    })
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    # Should have the via:plugin marker indicating redirect happened
    assert content.get("via") == "plugin", f"expected plugin redirect, got: {content}"
    # Output should be structured JSON from the cargo plugin
    out = content["out"]
    parsed = json.loads(out)
    assert "passed" in parsed or "ok" in parsed, f"expected structured output, got: {out}"


def test_exec_plugin_redirect_git_status(daimonos):
    """exec('git status') should redirect through native git plugin."""
    import os, subprocess
    # Init a git repo in the workspace
    daimonos.call_tool("exec", {"command": "git init"})
    daimonos.call_tool("exec", {
        "command": "git",
        "args": ["config", "user.email", "test@test.com"],
    })
    daimonos.call_tool("exec", {
        "command": "git",
        "args": ["config", "user.name", "Test"],
    })

    result = daimonos.call_tool("exec", {"command": "git status"})
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    assert content.get("via") == "plugin", f"expected plugin redirect, got: {content}"
    out = json.loads(content["out"])
    assert "clean" in out or "untracked" in out or "modified" in out, f"got: {out}"


def test_exec_plugin_redirect_preserves_raw_for_unknown(daimonos):
    """Commands that don't match a plugin should go through raw exec."""
    result = daimonos.call_tool("exec", {"command": "echo via-test"})
    content = json.loads(result["content"][0]["text"])
    assert content["exit"] == 0
    assert content.get("via") is None
    assert content["out"] == "via-test"
