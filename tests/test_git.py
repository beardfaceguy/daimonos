"""Tests for the git_status, git_log, git_diff, and git_branch MCP tools."""

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


def test_git_status_clean_repo(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    data = _parse(daimonos.call_tool("git_status"))
    assert data["clean"] is True
    assert data["modified"] == []
    assert data["untracked"] == []


def test_git_status_modified_file(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("changed")

    data = _parse(daimonos.call_tool("git_status"))
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

    data = _parse(daimonos.call_tool("git_status"))
    assert "new_file.txt" in data["untracked"]


def test_git_status_not_a_repo(daimonos):
    result = daimonos.call_tool("git_status")
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

    data = _parse(daimonos.call_tool("git_log"))
    assert data["count"] == 2
    commits = data["commits"]
    assert commits[0]["msg"] == "second commit"
    assert commits[1]["msg"] == "first commit"
    assert commits[0]["author"] == "Test User"
    assert len(commits[0]["hash"]) >= 40


def test_git_log_with_limit(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    for i in range(5):
        with open(os.path.join(ws, "file.txt"), "w") as f:
            f.write(f"v{i}")
        _git(ws, "add", ".")
        _git(ws, "commit", "-m", f"commit {i}")

    data = _parse(daimonos.call_tool("git_log", {"limit": 2}))
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

    data = _parse(daimonos.call_tool("git_log", {"path": "a.txt"}))
    assert data["count"] == 1
    assert data["commits"][0]["msg"] == "add a"


def test_git_diff_unstaged(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original\n")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("modified\n")

    data = _parse(daimonos.call_tool("git_diff"))
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

    data = _parse(daimonos.call_tool("git_diff", {"mode": "staged"}))
    assert data["staged"] is True
    assert data["file_count"] >= 1


def test_git_diff_clean_repo(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content\n")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    data = _parse(daimonos.call_tool("git_diff"))
    assert data["file_count"] == 0
    assert data["files"] == []


def test_git_branch_current(daimonos):
    ws = daimonos.workspace
    _init_repo(ws)

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("x")
    _git(ws, "add", ".")
    _git(ws, "commit", "-m", "init")

    data = _parse(daimonos.call_tool("git_branch"))
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

    data = _parse(daimonos.call_tool("git_branch"))
    assert data["current"] == "main"
    assert data["count"] == 3
    branches = data["branches"]
    assert "main" in branches
    assert "feature" in branches
    assert "bugfix" in branches


def test_tools_list_includes_git_tools(daimonos):
    """Git tools are visible in the initial tool listing."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "diff_files" in tool_names
    assert "git_status" in tool_names
    assert "git_log" in tool_names
    assert "git_diff" in tool_names
    assert "git_branch" in tool_names
