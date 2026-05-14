"""Regression tests for set_cwd (vikunja #249, fix #7).

The bug: `set_cwd` checked `is_dir()` before `canonicalize()`. Two syscalls
created a TOCTOU window where the path could be retargeted between checks,
and a non-existent path produced the misleading "not a directory" error
instead of a clear "resolve path" failure.

After the fix, `canonicalize()` runs first; `is_dir()` is checked on the
canonical path, eliminating the TOCTOU window and producing accurate
error messages.
"""
from __future__ import annotations

import os
import pytest


def _err_text(res: dict) -> str:
    assert res.get("isError") is True, f"expected isError=true, got: {res!r}"
    content = res.get("content", [])
    assert content, f"expected content list with error message, got: {res!r}"
    return content[0].get("text", "")


def test_set_cwd_missing_path_returns_canonicalize_error(daimonos):
    """Non-existent path must produce a canonicalize/resolve error,
    not a 'not a directory' error from is_dir() on a missing path.

    Before the fix, is_dir() returned false on the non-existent path and
    we reported "not a directory: ...". After the fix, canonicalize() runs
    first and fails with NotFound, producing a "resolve path: ..." error
    that correctly identifies the path doesn't exist.
    """
    res = daimonos.call_tool("set_cwd", {"path": "does_not_exist_anywhere"})
    text = _err_text(res)
    assert "resolve" in text.lower(), (
        f"expected 'resolve path: ...' style error from canonicalize, "
        f"got: {text!r}"
    )


def test_set_cwd_on_file_uses_canonical_path_in_error(daimonos, tmp_path):
    """When set_cwd is given a path to a regular file, the error must
    reference the canonical absolute path produced by canonicalize(), not
    the user-supplied input. This proves canonicalize ran first (closing
    the TOCTOU window).
    """
    f = tmp_path / "actually_a_file.txt"
    f.write_text("hi")
    res = daimonos.call_tool("set_cwd", {"path": "actually_a_file.txt"})
    text = _err_text(res)
    canonical = str(f.resolve())
    assert canonical in text, (
        f"expected canonical path {canonical!r} in error, got: {text!r}"
    )


def test_set_cwd_on_symlink_to_file_resolves_via_canonicalize(daimonos, tmp_path):
    """When set_cwd is given a symlink pointing at a file, the error must
    reference the symlink target after canonicalize, not the symlink path.
    This is the strongest signal that canonicalize ran first.
    """
    real_file = tmp_path / "real_file.txt"
    real_file.write_text("hi")
    link = tmp_path / "link_to_file"
    try:
        os.symlink(real_file, link)
    except OSError:
        pytest.skip("symlinks not supported on this filesystem")
    res = daimonos.call_tool("set_cwd", {"path": "link_to_file"})
    text = _err_text(res)
    canonical_target = str(real_file.resolve())
    assert canonical_target in text, (
        f"expected symlink target {canonical_target!r} in error, got: {text!r}"
    )


def test_set_cwd_on_directory_succeeds(daimonos, tmp_path):
    """Sanity check: setting cwd to a real subdirectory should succeed
    and report the canonical path in the response."""
    subdir = tmp_path / "good_dir"
    subdir.mkdir()
    res = daimonos.call_tool("set_cwd", {"path": "good_dir"})
    assert res.get("isError") is not True, f"unexpected error: {res!r}"
    text = res["content"][0]["text"]
    assert str(subdir.resolve()) in text, (
        f"expected canonical cwd {subdir.resolve()!r} in response, got: {text!r}"
    )
