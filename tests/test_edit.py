"""Tests for edit_file MCP tool."""

import json


def test_edit_applies_replacements(daimonos):
    daimonos.call_tool("write_file", {
        "path": "edit_target.txt",
        "content": "hello world foo bar",
    })

    result = daimonos.call_tool("edit_file", {
        "path": "edit_target.txt",
        "edits": ["hello", "goodbye", "foo", "baz"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 2

    result = daimonos.call_tool("read_file", {"path": "edit_target.txt"})
    text = json.loads(result["content"][0]["text"])["content"]
    assert text == "goodbye world baz bar"


def test_edit_no_match(daimonos):
    daimonos.call_tool("write_file", {
        "path": "nomatch.txt",
        "content": "abc def",
    })

    result = daimonos.call_tool("edit_file", {
        "path": "nomatch.txt",
        "edits": ["xyz", "123"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 0


def test_edit_multiple_pairs(daimonos):
    daimonos.call_tool("write_file", {
        "path": "multi.txt",
        "content": "alpha beta gamma",
    })

    result = daimonos.call_tool("edit_file", {
        "path": "multi.txt",
        "edits": ["alpha", "one", "beta", "two", "gamma", "three"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 3

    result = daimonos.call_tool("read_file", {"path": "multi.txt"})
    text = json.loads(result["content"][0]["text"])["content"]
    assert text == "one two three"


def test_edit_nonexistent_file(daimonos):
    result = daimonos.call_tool("edit_file", {
        "path": "does_not_exist.txt",
        "edits": ["a", "b"],
    })
    assert result.get("isError") is True


def test_edit_returns_diffs(daimonos):
    """edit_file should return a diffs array confirming each applied change."""
    daimonos.call_tool("write_file", {
        "path": "diffs.txt",
        "content": "hello world foo bar",
    })

    result = daimonos.call_tool("edit_file", {
        "path": "diffs.txt",
        "edits": ["hello", "goodbye", "foo", "baz"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 2
    assert len(content["diffs"]) == 2
    assert content["diffs"][0] == ["hello", "goodbye"]
    assert content["diffs"][1] == ["foo", "baz"]


def test_edit_no_diffs_when_nothing_matches(daimonos):
    """When no edits match, diffs should be absent."""
    daimonos.call_tool("write_file", {
        "path": "nodiffs.txt",
        "content": "abc",
    })

    result = daimonos.call_tool("edit_file", {
        "path": "nodiffs.txt",
        "edits": ["xyz", "123"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 0
    assert "diffs" not in content
