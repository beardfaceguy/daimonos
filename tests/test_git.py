"""Tests for the unified git MCP tool."""

import json
import os
import subprocess


def _parse(result):
    text = result["content"][0]["text"]
    return json.loads(text)


def _git(ws, *args):
    subprocess.run(["git"] + list(args), cwd=ws, capture_output=True, check=True)


def _init_repo(ws):
    _git(ws, "init", "-b", "main")
    _git(ws, "config", "user.email", "test@test.com")
    _git(ws, "config", "user.name", "Test User")
    _git(ws, "config", "commit.gpgsign", "false")


def test_git_status_clean_repo(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    data = _parse(daimonos.call_tool("git", {"command": "status"}))
    assert data["clean"] is True
    assert "modified" not in data, "empty arrays should be omitted"
    assert "untracked" not in data, "empty arrays should be omitted"


def test_git_status_modified_file(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("changed")

    data = _parse(daimonos.call_tool("git", {"command": "status"}))
    assert data["clean"] is False
    assert any("file.txt" in f for f in data["modified"])


def test_git_status_untracked_file(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "tracked.txt"), "w") as f:
        f.write("tracked")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    with open(os.path.join(ws, "new_file.txt"), "w") as f:
        f.write("untracked")

    data = _parse(daimonos.call_tool("git", {"command": "status"}))
    assert "new_file.txt" in data["untracked"]


def test_git_status_not_a_repo(daimonos):
    result = daimonos.call_tool("git", {"command": "status"})
    assert result.get("isError") is True


def test_git_log_returns_commits(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("v1")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "first commit")

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("v2")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "second commit")

    data = _parse(daimonos.call_tool("git", {"command": "log"}))
    assert data["count"] == 2
    commits = data["commits"]
    assert commits[0]["m"] == "second commit"
    assert commits[1]["m"] == "first commit"
    assert commits[0]["a"] == "Test User"
    assert len(commits[0]["h"]) >= 7


def test_git_log_with_limit(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    for i in range(5):
        with open(os.path.join(ws, "file.txt"), "w") as f:
            f.write(f"v{i}")
        _git(ws, "add", ".")
        _git(ws, "commit", "-m", f"commit {i}")

    data = _parse(daimonos.call_tool("git", {"command": "log", "limit": 2}))
    assert data["count"] == 2


def test_git_log_with_path_filter(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "a.txt"), "w") as f:
        f.write("a")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "add a")

    with open(os.path.join(ws, "b.txt"), "w") as f:
        f.write("b")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "add b")

    data = _parse(daimonos.call_tool("git", {"command": "log", "path": "a.txt"}))
    assert data["count"] == 1
    assert data["commits"][0]["m"] == "add a"


def test_git_diff_unstaged(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original\n")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("modified\n")

    data = _parse(daimonos.call_tool("git", {"command": "diff"}))
    assert data["staged"] is False
    assert data["file_count"] >= 1
    files = data["files"]
    assert any("file.txt" in f["f"] for f in files)


def test_git_diff_staged(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original\n")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("staged_change\n")
    _git(ws, "add", ".")

    data = _parse(daimonos.call_tool("git", {"command": "diff", "mode": "staged"}))
    assert data["staged"] is True
    assert data["file_count"] >= 1


def test_git_diff_clean_repo(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content\n")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    data = _parse(daimonos.call_tool("git", {"command": "diff"}))
    assert data["file_count"] == 0
    assert data["files"] == []


def test_git_branch_current(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("x")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    data = _parse(daimonos.call_tool("git", {"command": "branch"}))
    assert data["current"] == "main"
    assert "main" in data["branches"]


def test_git_branch_multiple(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("x")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    _git(ws, "branch", "feature")
    _git(ws, "branch", "bugfix")

    data = _parse(daimonos.call_tool("git", {"command": "branch"}))
    assert data["current"] == "main"
    assert data["count"] == 3
    branches = data["branches"]
    assert "main" in branches
    assert "feature" in branches
    assert "bugfix" in branches


def test_tools_list_includes_git(daimonos):
    """Unified git tool is visible in the initial tool listing."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "git" in tool_names
    assert "git_status" not in tool_names, "individual git tools should be merged"


def test_tools_list_hides_extended(daimonos):
    """Extended tools like diff_files, tool_pipeline, ls are not in initial listing."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "diff_files" not in tool_names
    assert "tool_pipeline" not in tool_names
    assert "tool_repair" not in tool_names
    assert "ls" not in tool_names, "ls should be behind list_all_tools"
