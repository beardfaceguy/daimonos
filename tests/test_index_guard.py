"""Regression tests for the over-broad root gate (vikunja #47).

daimonos gates eager indexing on a signal, not a path blocklist: a root
larger than the `max_files` preflight budget is only crawled if it looks like
a real project (a project marker such as `.git` / `Cargo.toml` at the root).
This stops an editor that inherited an over-broad cwd (commonly `$HOME`, but
equally a NAS mount or downloads dir) from indexing gigabytes of unrelated
files (observed at ~1.3 GB RSS for a single instance). Small roots are always
indexed; the filesystem root is always skipped.

The tests force a tiny budget via a config file so the "large" cases don't
require creating 50k files.
"""

import json
import os
import subprocess
import time

from conftest import DaimonosClient

SMALL_BUDGET_CONFIG = "[index]\nmax_files = 3\n"


def _spawn(binary, *, args, cwd):
    proc = subprocess.Popen(
        [binary, "--mcp", *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=cwd,
    )
    client = DaimonosClient(proc, cwd)
    init_resp = client.send_raw({
        "jsonrpc": "2.0",
        "id": client._next_id(),
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1.0.0"},
        },
    })
    assert "result" in init_resp, f"initialize failed: {init_resp}"
    client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})
    return proc, client


def _teardown(proc):
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def _index_files(client):
    info = json.loads(client.call_tool("workspace_info", {})["content"][0]["text"])
    return info["index"]["files"]


def _write_files(d, n):
    for i in range(n):
        with open(os.path.join(d, f"f{i}.rs"), "w") as fh:
            fh.write(f"fn f{i}() {{}} // searchable content {i}\n")


def _write_config(tmp_path):
    cfg = str(tmp_path / "daimonos.toml")
    with open(cfg, "w") as fh:
        fh.write(SMALL_BUDGET_CONFIG)
    return cfg


def test_large_unmarked_root_is_not_indexed(daimonos_binary, tmp_path):
    """8 files, budget 3, no project marker -> empty index."""
    proj = str(tmp_path / "proj")
    os.makedirs(proj)
    _write_files(proj, 8)
    cfg = _write_config(tmp_path)

    proc, client = _spawn(daimonos_binary, args=["-w", proj, "-c", cfg], cwd=proj)
    try:
        time.sleep(2.0)  # give any (incorrect) reindex time to run
        assert _index_files(client) == 0, "large unmarked root must not be indexed"
    finally:
        _teardown(proc)


def test_large_marked_root_is_indexed(daimonos_binary, tmp_path):
    """Same oversized tree, but a .git dir marks it as a real project."""
    proj = str(tmp_path / "proj")
    os.makedirs(proj)
    _write_files(proj, 8)
    os.makedirs(os.path.join(proj, ".git"))
    cfg = _write_config(tmp_path)

    proc, client = _spawn(daimonos_binary, args=["-w", proj, "-c", cfg], cwd=proj)
    try:
        deadline = time.time() + 8.0
        files = 0
        while time.time() < deadline:
            files = _index_files(client)
            if files > 0:
                break
            time.sleep(0.25)
        assert files > 0, "marked project must be indexed"
        assert files <= 3, f"index must respect max_files cap, got {files}"
    finally:
        _teardown(proc)


def test_small_root_is_indexed(daimonos_binary, tmp_path):
    """A small root (within the default budget) is indexed with no marker."""
    proj = str(tmp_path / "proj")
    os.makedirs(proj)
    _write_files(proj, 2)

    proc, client = _spawn(daimonos_binary, args=["-w", proj], cwd=proj)
    try:
        deadline = time.time() + 8.0
        files = 0
        while time.time() < deadline:
            files = _index_files(client)
            if files >= 2:
                break
            time.sleep(0.25)
        assert files >= 2, "small root should be indexed without a marker"
    finally:
        _teardown(proc)
