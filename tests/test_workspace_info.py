"""Tests for workspace_info MCP tool."""

import json


def test_workspace_info_has_session(daimonos):
    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    assert "session" in content
    session = content["session"]
    assert "workspace" in session
    assert "cwd" in session
    assert "env_keys" in session
    assert "bg_count" in session


def test_workspace_info_has_root_listing(daimonos):
    daimonos.call_tool("write_file", {"path": "marker.txt", "content": "x"})

    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    assert "root_listing" in content
    listing = content["root_listing"]
    assert "entries" in listing
    names = [e["n"] for e in listing["entries"]]
    assert "marker.txt" in names


def test_workspace_info_has_index(daimonos):
    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    assert "index" in content
    idx = content["index"]
    assert "files" in idx
    assert "trigrams" in idx
