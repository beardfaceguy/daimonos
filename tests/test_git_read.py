"""Pinned, read-only git access over MCP (ADR-017)."""

import json
import subprocess
from pathlib import Path


def git(repo, *args):
    return subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout.strip()


def repository(workspace):
    repo = Path(workspace) / "source"
    repo.mkdir()
    git(repo, "init", "-b", "main")
    git(repo, "config", "user.name", "Reviewer Fixture")
    git(repo, "config", "user.email", "review@example.test")
    git(repo, "config", "commit.gpgsign", "false")
    (repo / "file.txt").write_text("first line\nsecond line\n")
    git(repo, "add", ".")
    git(repo, "commit", "-m", "Initial source")
    return repo, git(repo, "rev-parse", "HEAD")


def test_git_read_resolves_nested_repository(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command": "resolve", "repo": "source", "rev": "HEAD"})
    assert not response.get("isError"), response
    resolved = json.loads(response["content"][0]["text"])
    assert resolved["commits"] == [commit]
    assert resolved["object_type"] == "commit"
    assert resolved["repo"].endswith("/source")


def test_git_read_show_file_is_pinned_and_paginated(daimonos):
    repo, commit = repository(daimonos.workspace)
    (repo / "file.txt").write_text("uncommitted content\n")
    response = daimonos.call_tool("git_read", {"command": "show_file", "repo": "source", "rev": commit, "path": "file.txt", "offset": 1, "limit": 1})
    assert not response.get("isError"), response
    document = json.loads(response["content"][0]["text"])
    assert document["content"] == "second line\n"
    assert document["commits"] == [commit]


def test_git_read_lists_tree_entries(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command":"list_tree", "repo":"source"})
    assert not response.get("isError"), response
    tree = json.loads(response["content"][0]["text"])
    assert tree["commits"] == [commit]
    assert tree["entries"] == [{"name":"file.txt", "type":"blob", "mode":"100644", "size":23, "sha":git(Path(daimonos.workspace)/"source", "rev-parse", "HEAD:file.txt")}]


def test_git_read_grep_returns_pinned_lines(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command":"grep", "repo":"source", "pattern":"second"})
    assert not response.get("isError"), response
    found = json.loads(response["content"][0]["text"])
    assert found["commits"] == [commit]
    assert found["matches"] == [{"path":"file.txt", "line_number":2, "line":"second line"}]


def test_git_read_log_has_full_metadata(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command":"log", "repo":"source", "limit":1})
    assert not response.get("isError"), response
    history = json.loads(response["content"][0]["text"])
    assert history["entries"][0]["sha"] == commit
    assert history["entries"][0]["author"] == "Reviewer Fixture"
    assert history["entries"][0]["subject"] == "Initial source"
    assert history["entries"][0]["author_date"]
    assert history["entries"][0]["committer_date"]


def test_git_read_diff_compares_commits_not_worktree(daimonos):
    repo, first = repository(daimonos.workspace)
    (repo/"file.txt").write_text("replacement\n")
    git(repo,"add",".")
    git(repo,"commit","-m","Second source")
    second=git(repo,"rev-parse","HEAD")
    response=daimonos.call_tool("git_read",{"command":"diff","repo":"source","a":first,"b":second,"mode":"stat"})
    assert not response.get("isError"), response
    diff=json.loads(response["content"][0]["text"])
    assert diff["commits"] == [first,second]
    assert diff["files"] == [{"path":"file.txt","added":1,"deleted":2}]


def test_git_read_show_commit_includes_root_changes(daimonos):
    _, commit = repository(daimonos.workspace)
    response=daimonos.call_tool("git_read",{"command":"show_commit","repo":"source","rev":commit,"patch":True})
    assert not response.get("isError"), response
    change=json.loads(response["content"][0]["text"])
    assert change["commit"]["sha"] == commit
    assert change["files"] == [{"path":"file.txt","added":2,"deleted":0}]
    assert "+first line" in change["patch"]


def test_git_read_refs_include_annotated_tags(daimonos):
    repo, commit = repository(daimonos.workspace)
    git(repo,"tag","-a","review","-m","Review snapshot")
    response=daimonos.call_tool("git_read",{"command":"refs","repo":"source"})
    assert not response.get("isError"), response
    refs=json.loads(response["content"][0]["text"])
    assert {r["name"] for r in refs["refs"]} == {"refs/heads/main","refs/tags/review"}
    assert all(r["commit_sha"] == commit for r in refs["refs"])


def test_git_read_ancestry_returns_merge_base(daimonos):
    repo, first = repository(daimonos.workspace)
    git(repo,"commit","--allow-empty","-m","Next")
    second=git(repo,"rev-parse","HEAD")
    response=daimonos.call_tool("git_read",{"command":"ancestry","repo":"source","a":first,"b":second})
    assert not response.get("isError"), response
    ancestry=json.loads(response["content"][0]["text"])
    assert ancestry["commits"] == [first,second]
    assert ancestry["is_ancestor"] is True
    assert ancestry["merge_base"] == first


def test_git_read_status_reports_dirty_state(daimonos):
    repo, commit = repository(daimonos.workspace)
    (repo/"file.txt").write_text("dirty\n")
    response=daimonos.call_tool("git_read",{"command":"worktree_status","repo":"source"})
    assert not response.get("isError"), response
    status=json.loads(response["content"][0]["text"])
    assert status["commits"] == [commit]
    assert status["entries"] == [{"status":" M","path":"file.txt"}]


def test_git_read_blame_uses_committed_lines(daimonos):
    _, commit = repository(daimonos.workspace)
    response=daimonos.call_tool("git_read",{"command":"blame","repo":"source","path":"file.txt","start_line":2,"end_line":2})
    assert not response.get("isError"), response
    blame=json.loads(response["content"][0]["text"])
    assert blame["commits"] == [commit]
    assert blame["lines"][0]["line_number"] == 2
    assert blame["lines"][0]["sha"] == commit
    assert blame["lines"][0]["author"] == "Reviewer Fixture"
    assert blame["lines"][0]["line"] == "second line"
    assert blame["lines"][0]["date"]


def test_git_read_is_listed_with_a_closed_schema(daimonos_factory, tmp_path):
    (tmp_path / "daimonos.toml").write_text("[mcp]\nfull_tool_schemas=true\n")
    daimonos = daimonos_factory(tmp_path)
    daimonos.call_tool("list_all_tools", {})
    tool = next(t for t in daimonos.list_tools() if t["name"] == "git_read")
    schema = tool["inputSchema"]
    assert schema["type"] == "object"
    assert schema["required"] == ["command"]
    assert schema["additionalProperties"] is False
    properties = schema["properties"]
    assert len(properties["command"]["enum"]) == 11
    assert properties["repo"]["type"] == "string"
    assert properties["paths"]["items"]["type"] == "string"
    assert properties["offset"]["type"] == "integer"
    response = daimonos.call_tool("git_read", {"command":"raw", "args":["status"]})
    assert response.get("isError")
    assert json.loads(response["content"][0]["text"])["error"]["kind"] == "rejected_argument"


def test_git_read_workspace_does_not_follow_session_cwd(daimonos):
    _, commit = repository(daimonos.workspace)
    nested = Path(daimonos.workspace) / "other"
    nested.mkdir()
    daimonos.call_tool("set_cwd", {"path":str(nested)})
    response = daimonos.call_tool("git_read", {"command":"resolve", "repo":"source"})
    assert not response.get("isError"), response
    assert json.loads(response["content"][0]["text"])["commits"] == [commit]
