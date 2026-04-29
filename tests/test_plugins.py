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
