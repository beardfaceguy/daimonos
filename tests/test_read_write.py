"""Tests for read_file and write_file MCP tools."""

import json
import os


def test_write_and_read_roundtrip(daimonos):
    result = daimonos.call_tool("write_file", {
        "path": "hello.txt",
        "content": "line1\nline2\nline3",
    })
    content = json.loads(result["content"][0]["text"])
    assert content["ok"] is True

    result = daimonos.call_tool("read_file", {"path": "hello.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "line1\nline2\nline3"
    assert content["lines"] == 3


def test_read_with_offset_and_limit(daimonos):
    daimonos.call_tool("write_file", {
        "path": "lines.txt",
        "content": "a\nb\nc\nd\ne",
    })

    result = daimonos.call_tool("read_file", {
        "path": "lines.txt",
        "offset": 1,
        "limit": 2,
    })
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "b\nc"
    assert content["returned"] == 2
    assert content["offset"] == 1


def test_read_nonexistent_file(daimonos):
    result = daimonos.call_tool("read_file", {"path": "nonexistent.txt"})
    assert result.get("isError") is True


def test_write_creates_nested_dirs(daimonos):
    result = daimonos.call_tool("write_file", {
        "path": "a/b/c/deep.txt",
        "content": "deep content",
    })
    content = json.loads(result["content"][0]["text"])
    assert content["ok"] is True

    result = daimonos.call_tool("read_file", {"path": "a/b/c/deep.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "deep content"


def test_write_overwrite(daimonos):
    daimonos.call_tool("write_file", {"path": "ow.txt", "content": "first"})
    daimonos.call_tool("write_file", {"path": "ow.txt", "content": "second"})

    result = daimonos.call_tool("read_file", {"path": "ow.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "second"


def test_read_dedup_returns_unchanged(daimonos):
    """Second read of same unchanged file returns {unchanged: true}."""
    daimonos.call_tool("write_file", {"path": "dedup.txt", "content": "hello\nworld"})

    r1 = json.loads(daimonos.call_tool("read_file", {"path": "dedup.txt"})["content"][0]["text"])
    assert r1["content"] == "hello\nworld"
    assert "unchanged" not in r1

    r2 = json.loads(daimonos.call_tool("read_file", {"path": "dedup.txt"})["content"][0]["text"])
    assert r2["unchanged"] is True
    assert r2["lines"] == 2
    assert "content" not in r2


def test_read_dedup_invalidated_by_write(daimonos):
    """After write_file, re-read returns full content."""
    daimonos.call_tool("write_file", {"path": "dw.txt", "content": "v1"})
    daimonos.call_tool("read_file", {"path": "dw.txt"})

    daimonos.call_tool("write_file", {"path": "dw.txt", "content": "v2"})

    r = json.loads(daimonos.call_tool("read_file", {"path": "dw.txt"})["content"][0]["text"])
    assert r["content"] == "v2"
    assert "unchanged" not in r


def test_read_dedup_invalidated_by_edit(daimonos):
    """After edit_file, re-read returns full content."""
    daimonos.call_tool("write_file", {"path": "de.txt", "content": "hello world"})
    daimonos.call_tool("read_file", {"path": "de.txt"})

    daimonos.call_tool("edit_file", {
        "path": "de.txt",
        "edits": ["hello", "goodbye"],
    })

    r = json.loads(daimonos.call_tool("read_file", {"path": "de.txt"})["content"][0]["text"])
    assert r["content"] == "goodbye world"
    assert "unchanged" not in r


def test_read_dedup_partial_read_bypasses_cache(daimonos):
    """Paginated reads always return content, never 'unchanged'."""
    daimonos.call_tool("write_file", {"path": "partial.txt", "content": "a\nb\nc"})
    daimonos.call_tool("read_file", {"path": "partial.txt"})

    r = json.loads(daimonos.call_tool("read_file", {
        "path": "partial.txt", "offset": 1, "limit": 1,
    })["content"][0]["text"])
    assert r["content"] == "b"
    assert "unchanged" not in r


# --- Trailing newline preservation (vikunja #246, fix #4) ---
#
# `str::lines()` strips trailing newlines, and `lines.join("\n")` does not
# restore them. The pre-fix `read()` collected lines and joined, silently
# losing the final `\n`. Any agent doing read → modify → write was producing
# round-trip-unsafe writes. These tests cover full reads, slice-to-EOF reads,
# and ensure no spurious newline is added when the original lacked one.


def test_read_full_preserves_trailing_newline(daimonos):
    """Full read of a file ending in '\\n' must return content ending in '\\n'."""
    daimonos.call_tool("write_file", {"path": "trail.txt", "content": "a\nb\nc\n"})
    r = json.loads(daimonos.call_tool("read_file", {"path": "trail.txt"})["content"][0]["text"])
    assert r["content"] == "a\nb\nc\n", repr(r["content"])
    assert r["lines"] == 3


def test_read_full_does_not_add_trailing_newline_when_absent(daimonos):
    """Full read of a file WITHOUT trailing newline must not gain one."""
    daimonos.call_tool("write_file", {"path": "notrail.txt", "content": "a\nb\nc"})
    r = json.loads(daimonos.call_tool("read_file", {"path": "notrail.txt"})["content"][0]["text"])
    assert r["content"] == "a\nb\nc", repr(r["content"])


def test_read_write_byte_identical_round_trip(daimonos):
    """The canonical regression for vikunja #246: read then write must be
    byte-identical when the file ends with a newline."""
    original = "alpha\nbeta\ngamma\n"
    daimonos.call_tool("write_file", {"path": "rt.txt", "content": original})
    r = json.loads(daimonos.call_tool("read_file", {"path": "rt.txt"})["content"][0]["text"])
    daimonos.call_tool("write_file", {"path": "rt2.txt", "content": r["content"]})
    r2 = json.loads(daimonos.call_tool("read_file", {"path": "rt2.txt"})["content"][0]["text"])
    assert r2["content"] == original, (
        f"round-trip lost the trailing newline: {r2['content']!r} != {original!r}"
    )


def test_read_offset_to_eof_preserves_trailing_newline(daimonos):
    """Offset read that reaches EOF must keep the file's trailing newline."""
    daimonos.call_tool(
        "write_file", {"path": "off.txt", "content": "x\ny\nz\nw\n"},
    )
    r = json.loads(
        daimonos.call_tool("read_file", {"path": "off.txt", "offset": 2})["content"][0]["text"]
    )
    assert r["content"] == "z\nw\n", repr(r["content"])


def test_read_limited_slice_omits_trailing_newline_when_not_at_eof(daimonos):
    """Limited read that does NOT reach EOF must not append a newline."""
    daimonos.call_tool(
        "write_file", {"path": "lim.txt", "content": "x\ny\nz\nw\n"},
    )
    r = json.loads(
        daimonos.call_tool(
            "read_file", {"path": "lim.txt", "offset": 1, "limit": 2}
        )["content"][0]["text"]
    )
    assert r["content"] == "y\nz", repr(r["content"])
    assert r["returned"] == 2


def test_read_limited_slice_to_eof_preserves_trailing_newline(daimonos):
    """Limited read whose slice happens to end at EOF must keep the newline."""
    daimonos.call_tool(
        "write_file", {"path": "lim2.txt", "content": "x\ny\nz\n"},
    )
    r = json.loads(
        daimonos.call_tool(
            "read_file", {"path": "lim2.txt", "offset": 1, "limit": 2}
        )["content"][0]["text"]
    )
    assert r["content"] == "y\nz\n", repr(r["content"])
    assert r["returned"] == 2
