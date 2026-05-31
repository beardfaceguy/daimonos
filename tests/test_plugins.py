"""End-to-end MCP integration tests for cargo, gh, and docker plugins."""

from __future__ import annotations

import json
import os
import subprocess


def _parse(result):
    text = result["content"][0]["text"]
    return json.loads(text)


def _is_error(result):
    return result.get("isError", False)


# ============================================================
# Cargo plugin tests
# ============================================================


def _create_rust_project(ws, with_test=True):
    """Create a minimal Cargo project in the workspace."""
    os.makedirs(os.path.join(ws, "src"), exist_ok=True)

    with open(os.path.join(ws, "Cargo.toml"), "w") as f:
        f.write("""[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
""")

    code = 'pub fn add(a: i32, b: i32) -> i32 { a + b }\n'
    if with_test:
        code += """
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_add() { assert_eq!(add(2, 3), 5); }
}
"""
    with open(os.path.join(ws, "src", "lib.rs"), "w") as f:
        f.write(code)


def test_cargo_check_valid_project(daimonos):
    """cargo check succeeds on valid Rust code."""
    _create_rust_project(daimonos.workspace)
    result = daimonos.call_tool("cargo", {"command": "check"})
    data = _parse(result)
    assert data["ok"] is True, f"cargo check failed: {data}"
    assert "errors" not in data or len(data["errors"]) == 0


def test_cargo_check_with_error(daimonos):
    """cargo check returns structured errors for invalid code."""
    _create_rust_project(daimonos.workspace, with_test=False)
    with open(os.path.join(daimonos.workspace, "src", "lib.rs"), "w") as f:
        f.write("pub fn broken() -> i32 { let x: String = 42; x }\n")
    result = daimonos.call_tool("cargo", {"command": "check"})
    data = _parse(result)
    assert data["ok"] is False
    assert len(data.get("errors", [])) > 0
    err = data["errors"][0]
    assert "message" in err


def test_cargo_test_passing(daimonos):
    """cargo test returns structured pass/fail counts."""
    _create_rust_project(daimonos.workspace)
    result = daimonos.call_tool("cargo", {"command": "test"})
    data = _parse(result)
    assert data["ok"] is True
    assert data["passed"] >= 1
    assert data["failed"] == 0


def test_cargo_test_with_filter(daimonos):
    """cargo test with filter runs subset of tests."""
    _create_rust_project(daimonos.workspace)
    result = daimonos.call_tool("cargo", {"command": "test", "filter": "test_add"})
    data = _parse(result)
    assert data["ok"] is True
    assert data["passed"] >= 1


def test_cargo_test_lib_flag(daimonos):
    """cargo test --lib only runs library tests."""
    _create_rust_project(daimonos.workspace)
    result = daimonos.call_tool("cargo", {"command": "test", "lib": True})
    data = _parse(result)
    assert data["ok"] is True
    assert data["passed"] >= 1


def test_cargo_build_succeeds(daimonos):
    """cargo build returns structured output."""
    _create_rust_project(daimonos.workspace)
    result = daimonos.call_tool("cargo", {"command": "build"})
    data = _parse(result)
    assert data["ok"] is True


def test_cargo_fmt_check(daimonos):
    """cargo fmt --check returns formatting status."""
    _create_rust_project(daimonos.workspace, with_test=False)
    result = daimonos.call_tool("cargo", {"command": "fmt"})
    data = _parse(result)
    assert "formatted" in data


def test_cargo_clippy(daimonos):
    """cargo clippy returns structured diagnostics."""
    _create_rust_project(daimonos.workspace)
    result = daimonos.call_tool("cargo", {"command": "clippy"})
    data = _parse(result)
    assert "ok" in data


def test_cargo_tool_visible_with_manifest(daimonos):
    """cargo tool appears in tool listing when Cargo.toml exists."""
    _create_rust_project(daimonos.workspace)
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "cargo" in tool_names


def test_cargo_tool_hidden_without_manifest(daimonos):
    """cargo tool is hidden when no Cargo.toml in workspace."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "cargo" not in tool_names


# ============================================================
# GitHub CLI (gh) plugin tests
# ============================================================


def test_gh_pr_list(daimonos):
    """gh pr_list returns structured PR listing."""
    ws = daimonos.workspace
    subprocess.run(["git", "init", "-b", "main"], cwd=ws, capture_output=True, check=True)
    subprocess.run(["git", "config", "user.email", "test@test.com"], cwd=ws, capture_output=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=ws, capture_output=True)
    subprocess.run(
        ["git", "remote", "add", "origin", "https://github.com/beardfaceguy/daimonos.git"],
        cwd=ws, capture_output=True
    )

    result = daimonos.call_tool("gh", {"command": "pr_list", "limit": 3})
    data = _parse(result)
    assert "prs" in data
    assert "count" in data
    assert isinstance(data["prs"], list)


def test_gh_api_endpoint(daimonos):
    """gh api calls a GitHub API endpoint and returns JSON."""
    ws = daimonos.workspace
    subprocess.run(["git", "init", "-b", "main"], cwd=ws, capture_output=True, check=True)
    subprocess.run(
        ["git", "remote", "add", "origin", "https://github.com/beardfaceguy/daimonos.git"],
        cwd=ws, capture_output=True
    )

    result = daimonos.call_tool("gh", {"command": "api", "endpoint": "repos/beardfaceguy/daimonos"})
    data = _parse(result)
    assert "name" in data or "full_name" in data or "data" in data


def test_gh_tool_visible_in_git_repo(daimonos):
    """gh tool appears when workspace is a git repo."""
    ws = daimonos.workspace
    subprocess.run(["git", "init", "-b", "main"], cwd=ws, capture_output=True, check=True)
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "gh" in tool_names


def test_gh_tool_hidden_without_git(daimonos):
    """gh tool is hidden when workspace is not a git repo."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "gh" not in tool_names


# ============================================================
# Docker plugin tests
# ============================================================


def test_docker_ps(daimonos):
    """docker ps returns structured container list."""
    result = daimonos.call_tool("docker", {"command": "ps"})
    if _is_error(result):
        import pytest
        pytest.skip("docker not available or not running")
    data = _parse(result)
    assert "containers" in data
    assert isinstance(data["containers"], list)


def test_docker_images(daimonos):
    """docker images returns structured image list."""
    result = daimonos.call_tool("docker", {"command": "images"})
    if _is_error(result):
        import pytest
        pytest.skip("docker not available or not running")
    data = _parse(result)
    assert "images" in data
    assert isinstance(data["images"], list)


def test_docker_tool_always_visible(daimonos):
    """docker tool appears in listing regardless of workspace type."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "docker" in tool_names


# ============================================================
# Pytest plugin tests
# ============================================================


def _create_pytest_project(ws, body=None, name="test_plug.py"):
    """Drop a tests/ directory with a single test file in the workspace."""
    tests_dir = os.path.join(ws, "tests")
    os.makedirs(tests_dir, exist_ok=True)
    body = body if body is not None else (
        "def test_pass():\n    assert 1 + 1 == 2\n\n"
        "def test_str():\n    assert 'hi'.upper() == 'HI'\n"
    )
    with open(os.path.join(tests_dir, name), "w") as f:
        f.write(body)
    open(os.path.join(tests_dir, "__init__.py"), "w").close()


def _pytest_available():
    return subprocess.run(
        ["pytest", "--version"], capture_output=True
    ).returncode == 0


def test_pytest_run_passing(daimonos):
    """pytest run on all-passing tests reports passed count and ok=true."""
    if not _pytest_available():
        import pytest
        pytest.skip("pytest not on PATH")
    _create_pytest_project(daimonos.workspace)
    result = daimonos.call_tool("pytest", {"command": "run"})
    data = _parse(result)
    assert data["ok"] is True, f"expected ok, got: {data}"
    assert data["passed"] == 2
    assert data["failed"] == 0
    assert "failures" not in data or len(data["failures"]) == 0


def test_pytest_run_with_failure(daimonos):
    """pytest run reports structured failure ids when a test fails."""
    if not _pytest_available():
        import pytest
        pytest.skip("pytest not on PATH")
    _create_pytest_project(
        daimonos.workspace,
        body=(
            "def test_pass():\n    assert True\n\n"
            "def test_fail():\n    assert 1 == 2\n\n"
            "def test_skip():\n    import pytest\n    pytest.skip('not yet')\n"
        ),
    )
    result = daimonos.call_tool("pytest", {"command": "run"})
    data = _parse(result)
    assert data["ok"] is False
    assert data["passed"] == 1
    assert data["failed"] == 1
    assert data["skipped"] == 1
    failures = data.get("failures", [])
    assert len(failures) == 1
    assert "test_fail" in failures[0]


def test_pytest_run_with_filter(daimonos):
    """pytest run with -k filter selects a subset."""
    if not _pytest_available():
        import pytest
        pytest.skip("pytest not on PATH")
    _create_pytest_project(daimonos.workspace)
    result = daimonos.call_tool("pytest", {"command": "run", "filter": "test_pass"})
    data = _parse(result)
    assert data["ok"] is True
    assert data["passed"] == 1


def test_pytest_collect(daimonos):
    """pytest collect returns the list of discovered test ids."""
    if not _pytest_available():
        import pytest
        pytest.skip("pytest not on PATH")
    _create_pytest_project(daimonos.workspace)
    result = daimonos.call_tool("pytest", {"command": "collect"})
    data = _parse(result)
    assert data["ok"] is True
    assert len(data["tests"]) >= 2
    assert any("test_pass" in t for t in data["tests"])
    assert "count" not in data, "redundant count field must be omitted"


def test_pytest_tool_visible_with_tests_dir(daimonos):
    """pytest tool appears when a tests/ directory exists."""
    _create_pytest_project(daimonos.workspace)
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "pytest" in tool_names


def test_pytest_tool_hidden_without_python_context(daimonos):
    """pytest tool is hidden when no Python project markers are present."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "pytest" not in tool_names


def test_pytest_via_starlark(daimonos):
    """pytest tool works through execute_script Starlark binding."""
    if not _pytest_available():
        import pytest
        pytest.skip("pytest not on PATH")
    _create_pytest_project(daimonos.workspace)
    code = 'result = pytest("run")'
    result = daimonos.call_tool("execute_script", {"code": code})
    data = _parse(result)
    assert data["result"]["ok"] is True
    assert data["result"]["passed"] == 2


# ============================================================
# Discord plugin tests
# ============================================================


def test_discord_tool_visible(daimonos):
    """discord tool appears in listing (plugin registration is config-driven)."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "discord" in tool_names


def test_discord_disabled_by_default(daimonos):
    """discord calls fail with a clear error unless [discord].enabled is true."""
    result = daimonos.call_tool("discord", {"command": "list_guilds"})
    assert result.get("isError") is True
    text = result["content"][0]["text"]
    assert "disabled" in text


def test_discord_search_messages_disabled_by_default(daimonos):
    """search_messages is available on the tool surface but blocked when disabled."""
    result = daimonos.call_tool(
        "discord",
        {"command": "search_messages", "channel_id": "123456789012345678", "query": "deploy"},
    )
    assert result.get("isError") is True
    text = result["content"][0]["text"]
    assert "disabled" in text


# ============================================================
# Starlark integration tests
# ============================================================


def test_cargo_via_starlark(daimonos):
    """cargo tool works through execute_script Starlark binding."""
    _create_rust_project(daimonos.workspace)
    code = 'result = cargo("check")'
    result = daimonos.call_tool("execute_script", {"code": code})
    data = _parse(result)
    assert data["result"]["ok"] is True


def test_gh_via_starlark(daimonos):
    """gh tool works through execute_script Starlark binding."""
    ws = daimonos.workspace
    subprocess.run(["git", "init", "-b", "main"], cwd=ws, capture_output=True, check=True)
    subprocess.run(
        ["git", "remote", "add", "origin", "https://github.com/beardfaceguy/daimonos.git"],
        cwd=ws, capture_output=True
    )
    code = 'result = gh("pr_list", limit=1)'
    result = daimonos.call_tool("execute_script", {"code": code})
    data = _parse(result)
    assert "result" in data
