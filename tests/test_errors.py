"""Tests for error handling: missing args, unknown tools, invalid paths."""


def test_unknown_tool_returns_error(daimonos):
    """Calling a nonexistent tool should signal an error."""
    result = daimonos.call_tool("nonexistent_tool", {})
    assert result.get("isError") is True


def test_read_file_missing_path(daimonos):
    result = daimonos.call_tool("read_file", {})
    assert result.get("isError") is True


def test_write_file_missing_content(daimonos):
    result = daimonos.call_tool("write_file", {"path": "x.txt"})
    assert result.get("isError") is True


def test_edit_file_odd_edits(daimonos):
    """Edits array must have even length (old/new pairs)."""
    daimonos.call_tool("write_file", {"path": "odd.txt", "content": "abc"})
    result = daimonos.call_tool("edit_file", {
        "path": "odd.txt",
        "edits": ["a", "b", "leftover"],
    })
    assert result.get("isError") is True


def test_exec_missing_command(daimonos):
    result = daimonos.call_tool("exec", {})
    assert result.get("isError") is True


def test_read_outside_workspace(daimonos):
    """Absolute path outside workspace — should still work (no jail) but returns valid response."""
    result = daimonos.call_tool("read_file", {"path": "/etc/hostname"})
    # Just verify we get a response (not a crash), whether success or error
    assert "content" in result
