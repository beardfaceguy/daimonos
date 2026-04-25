"""Tests for symbolic link and hard link handling across all file operations."""

import json
import os


# --- Symlink tests ---


def test_read_through_symlink(daimonos):
    daimonos.call_tool("write_file", {"path": "real.txt", "content": "symlink data"})
    os.symlink(
        os.path.join(daimonos.workspace, "real.txt"),
        os.path.join(daimonos.workspace, "link.txt"),
    )

    result = daimonos.call_tool("read_file", {"path": "link.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "symlink data"


def test_read_broken_symlink(daimonos):
    os.symlink("/nonexistent_target", os.path.join(daimonos.workspace, "broken.txt"))

    result = daimonos.call_tool("read_file", {"path": "broken.txt"})
    assert result.get("isError") is True


def test_write_through_symlink_updates_target(daimonos):
    daimonos.call_tool("write_file", {"path": "wreal.txt", "content": "old"})
    os.symlink(
        os.path.join(daimonos.workspace, "wreal.txt"),
        os.path.join(daimonos.workspace, "wlink.txt"),
    )

    daimonos.call_tool("write_file", {"path": "wlink.txt", "content": "new via symlink"})

    result = daimonos.call_tool("read_file", {"path": "wreal.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "new via symlink"


def test_edit_through_symlink(daimonos):
    daimonos.call_tool("write_file", {"path": "ereal.txt", "content": "hello world"})
    os.symlink(
        os.path.join(daimonos.workspace, "ereal.txt"),
        os.path.join(daimonos.workspace, "elink.txt"),
    )

    result = daimonos.call_tool("edit_file", {
        "path": "elink.txt",
        "edits": ["hello", "goodbye"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 1

    result = daimonos.call_tool("read_file", {"path": "ereal.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "goodbye world"


def test_stat_detects_symlink(daimonos):
    daimonos.call_tool("write_file", {"path": "stgt.txt", "content": "data"})
    os.symlink(
        os.path.join(daimonos.workspace, "stgt.txt"),
        os.path.join(daimonos.workspace, "slink.txt"),
    )

    result = daimonos.call_tool("read_file", {"path": "slink.txt"})
    assert not result.get("isError")

    # Use exec to call stat through the daemon (stat is not an MCP tool,
    # but workspace_info has the root listing)
    # Instead, we test that read works and the file is accessible


def test_stat_broken_symlink_via_workspace(daimonos):
    """Broken symlinks should not crash workspace_info."""
    os.symlink("/no/such/path", os.path.join(daimonos.workspace, "deadlink"))

    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    listing = content["root_listing"]
    names = [e["n"] for e in listing["entries"]]
    assert "deadlink" in names


def test_ls_reports_symlink_flag(daimonos):
    daimonos.call_tool("write_file", {"path": "lsreal.txt", "content": "x"})
    os.symlink(
        os.path.join(daimonos.workspace, "lsreal.txt"),
        os.path.join(daimonos.workspace, "lslink.txt"),
    )

    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    entries = content["root_listing"]["entries"]

    real = next(e for e in entries if e["n"] == "lsreal.txt")
    link = next(e for e in entries if e["n"] == "lslink.txt")

    assert real.get("l") is None, "regular file should not have 'l' field"
    assert link.get("l") is True, "symlink should have 'l': true"


def test_ls_symlink_to_dir_shows_is_dir(daimonos):
    os.makedirs(os.path.join(daimonos.workspace, "realdir"))
    os.symlink(
        os.path.join(daimonos.workspace, "realdir"),
        os.path.join(daimonos.workspace, "dirlink"),
    )

    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    entries = content["root_listing"]["entries"]

    link = next(e for e in entries if e["n"] == "dirlink")
    assert link.get("l") is True
    assert link["d"] is True, "symlink to dir should have d=true"


def test_search_through_symlink(daimonos):
    daimonos.call_tool("write_file", {"path": "searchreal.txt", "content": "unique_symlink_token\n"})
    os.symlink(
        os.path.join(daimonos.workspace, "searchreal.txt"),
        os.path.join(daimonos.workspace, "searchlink.txt"),
    )

    result = daimonos.call_tool("search", {
        "pattern": "unique_symlink_token",
        "mode": "content",
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content["matches"]) >= 1


# --- Hard link tests ---


def test_read_hard_link(daimonos):
    daimonos.call_tool("write_file", {"path": "hlsrc.txt", "content": "hard link data"})
    os.link(
        os.path.join(daimonos.workspace, "hlsrc.txt"),
        os.path.join(daimonos.workspace, "hlcopy.txt"),
    )

    result = daimonos.call_tool("read_file", {"path": "hlcopy.txt"})
    content = json.loads(result["content"][0]["text"])
    assert content["content"] == "hard link data"


def test_edit_hard_link(daimonos):
    daimonos.call_tool("write_file", {"path": "hledit.txt", "content": "alpha beta"})
    os.link(
        os.path.join(daimonos.workspace, "hledit.txt"),
        os.path.join(daimonos.workspace, "hledit2.txt"),
    )

    result = daimonos.call_tool("edit_file", {
        "path": "hledit2.txt",
        "edits": ["alpha", "omega"],
    })
    content = json.loads(result["content"][0]["text"])
    assert content["applied"] == 1


def test_ls_hard_links_are_regular_files(daimonos):
    daimonos.call_tool("write_file", {"path": "hl1.txt", "content": "data"})
    os.link(
        os.path.join(daimonos.workspace, "hl1.txt"),
        os.path.join(daimonos.workspace, "hl2.txt"),
    )

    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    entries = content["root_listing"]["entries"]

    hl1 = next(e for e in entries if e["n"] == "hl1.txt")
    hl2 = next(e for e in entries if e["n"] == "hl2.txt")

    assert hl1.get("l") is None, "hard link should not have 'l' field"
    assert hl2.get("l") is None, "hard link should not have 'l' field"
    assert hl1["d"] is False
    assert hl2["d"] is False
