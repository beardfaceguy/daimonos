"""Tests for the diff_files MCP tool."""

import json
import os


def _parse(result):
    text = result["content"][0]["text"]
    return json.loads(text)


def test_diff_identical_files(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "a.txt"), "w") as f:
        f.write("hello\nworld\n")
    with open(os.path.join(ws, "b.txt"), "w") as f:
        f.write("hello\nworld\n")

    data = _parse(daimonos.call_tool("diff_files", {"path_a": "a.txt", "path_b": "b.txt"}))
    assert data["identical"] is True
    assert data["count"] == 0
    assert data["hunks"] == []


def test_diff_different_files(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "a.txt"), "w") as f:
        f.write("line1\nline2\nline3\n")
    with open(os.path.join(ws, "b.txt"), "w") as f:
        f.write("line1\nchanged\nline3\n")

    data = _parse(daimonos.call_tool("diff_files", {"path_a": "a.txt", "path_b": "b.txt"}))
    assert data["identical"] is False
    assert data["count"] >= 1

    hunks = data["hunks"]
    assert len(hunks) >= 1
    changes = hunks[0]["changes"]
    tags = [c["t"] for c in changes]
    assert "-" in tags
    assert "+" in tags
    deleted = [c for c in changes if c["t"] == "-"]
    assert any("line2" in c["v"] for c in deleted)


def test_diff_file_vs_content(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "orig.txt"), "w") as f:
        f.write("alpha\nbeta\n")

    data = _parse(daimonos.call_tool("diff_files", {
        "path_a": "orig.txt",
        "content_b": "alpha\ngamma\n",
    }))
    assert data["identical"] is False
    changes = data["hunks"][0]["changes"]
    tags = [c["t"] for c in changes]
    assert "-" in tags
    assert "+" in tags


def test_diff_missing_file(daimonos):
    result = daimonos.call_tool("diff_files", {"path_a": "nope.txt", "path_b": "also_nope.txt"})
    assert result.get("isError") is True


def test_diff_missing_both_args(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content")

    result = daimonos.call_tool("diff_files", {"path_a": "file.txt"})
    assert result.get("isError") is True


def test_diff_hunk_line_ranges(daimonos):
    ws = daimonos.workspace
    lines_a = [f"line{i}\n" for i in range(20)]
    lines_b = list(lines_a)
    lines_b[10] = "modified\n"

    with open(os.path.join(ws, "big_a.txt"), "w") as f:
        f.writelines(lines_a)
    with open(os.path.join(ws, "big_b.txt"), "w") as f:
        f.writelines(lines_b)

    data = _parse(daimonos.call_tool("diff_files", {"path_a": "big_a.txt", "path_b": "big_b.txt"}))
    assert data["identical"] is False
    hunk = data["hunks"][0]
    assert "old" in hunk
    assert "new" in hunk
    assert len(hunk["old"]) == 2
    assert len(hunk["new"]) == 2
