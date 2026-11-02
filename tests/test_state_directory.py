"""Acceptance tests for the caller-owned state root (ADR-021, WP01).

Context: docs/contexts/reviewer-runtime/CONTEXT.md; interface ledger
docs/contexts/reviewer-runtime/INTERFACES.md; decision
docs/adr/021-caller-owned-run-authority.md.

An explicit ``--state-dir`` must redirect every internal writer below that
private caller-owned root, even when HOME is unusable, with no HOME/XDG/temp
fallback. These tests exercise the behavior end-to-end over MCP.

WP01 covers MCP and native tool-serving modes. Other frontends reject the
selected state directory until WP02 relocates their remaining state.
"""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import stat
import subprocess

import pytest

from conftest import _handshake, _terminate


def _unusable_home(tmp_path):
    """A directory made read-only so any HOME-based write fails."""
    home = tmp_path / "unusable-home"
    home.mkdir()
    os.chmod(home, 0o500)
    return home


def _spawn(daimonos_binary, workspace, state_dir, home):
    env = {**os.environ, "HOME": str(home)}
    env.pop("XDG_CONFIG_HOME", None)
    env.pop("XDG_DATA_HOME", None)
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "--state-dir", str(state_dir), "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    return proc


def test_state_dir_redirects_coordination_write_with_unusable_home(
    daimonos_binary, tmp_path
):
    """register_agent writes the coordination DB; with --state-dir and an
    unwritable HOME it must land under the state root, and nothing may be
    created under HOME."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    home = _unusable_home(tmp_path)

    proc = _spawn(daimonos_binary, workspace, state_dir, home)
    try:
        client = _handshake(proc, workspace)
        result = client.call_tool(
            "register_agent", {"name": "StateProbe", "task": "probe"}
        )
        assert result, "register_agent returned no result"
    finally:
        _terminate(proc)

    coordination = state_dir / "coordination"
    dbs = list(coordination.glob("*.db")) if coordination.exists() else []
    assert dbs, f"expected a coordination DB under {coordination}, found none"

    # No daimonos state may have leaked into the unusable HOME.
    leaked = home / ".daimonos"
    assert not leaked.exists(), f"state leaked into HOME at {leaked}"


def test_state_dir_rejects_preexisting_coordination_database_symlink(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    coordination = state / "coordination"
    coordination.mkdir()
    workspace_id = hashlib.sha256(str(workspace.resolve()).encode()).hexdigest()
    outside = tmp_path / "outside.db"
    outside.write_bytes(b"")
    (coordination / f"{workspace_id}.db").symlink_to(outside)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert outside.read_bytes() == b""
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_preexisting_coordination_database_hardlink(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.db"
    evidence.write_bytes(b"")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    coordination = state / "coordination"
    coordination.mkdir()
    workspace_id = hashlib.sha256(str(workspace.resolve()).encode()).hexdigest()
    os.link(evidence, coordination / f"{workspace_id}.db")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert evidence.read_bytes() == b""
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_coordination_database_hardlink_to_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.db"
    connection = sqlite3.connect(evidence)
    connection.execute("CREATE TABLE original_data (value TEXT)")
    connection.commit()
    connection.close()
    original = evidence.read_bytes()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    coordination = state / "coordination"
    coordination.mkdir()
    workspace_id = hashlib.sha256(str(workspace.resolve()).encode()).hexdigest()
    proc = _spawn(daimonos_binary, workspace, state, tmp_path / "home")
    try:
        client = _handshake(proc, workspace)
        os.link(evidence, coordination / f"{workspace_id}.db")
        result = client.call_tool("register_agent", {"name": "StateProbe"})
    finally:
        _terminate(proc)

    assert evidence.read_bytes() == original, "coordination changed a workspace file through a hard link"
    assert result.get("isError"), result


def test_state_dir_rejects_coordination_database_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    coordination = state / "coordination"
    coordination.mkdir()
    workspace_id = hashlib.sha256(str(workspace.resolve()).encode()).hexdigest()
    outside = tmp_path / "outside.db"
    outside.write_bytes(b"")
    proc = _spawn(daimonos_binary, workspace, state, tmp_path / "home")
    try:
        client = _handshake(proc, workspace)
        (coordination / f"{workspace_id}.db").symlink_to(outside)
        result = client.call_tool("register_agent", {"name": "StateProbe"})
    finally:
        _terminate(proc)

    assert outside.read_bytes() == b"", "coordination wrote outside the selected state root"
    assert result.get("isError"), result


def test_missing_state_dir_fails_before_startup(daimonos_binary, tmp_path):
    """A non-existent --state-dir is rejected before writers; no fallback."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    missing = tmp_path / "nope"  # never created
    home = _unusable_home(tmp_path)

    env = {**os.environ, "HOME": str(home)}
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(missing), "-w", str(workspace)],
        input=b"",
        capture_output=True,
        timeout=15,
        env=env,
    )
    assert completed.returncode == 2, (
        f"expected exit 2 (run_configuration_invalid), got {completed.returncode}: "
        f"{completed.stderr.decode(errors='replace')}"
    )


def test_state_dir_inside_workspace_is_rejected(daimonos_binary, tmp_path):
    """A state root overlapping the workspace (evidence) tree is rejected."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    inside = workspace / "state"
    inside.mkdir(mode=0o700)
    home = _unusable_home(tmp_path)

    env = {**os.environ, "HOME": str(home)}
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(inside), "-w", str(workspace)],
        input=b"",
        capture_output=True,
        timeout=15,
        env=env,
    )
    assert completed.returncode == 2, (
        f"expected exit 2 for overlapping state root, got {completed.returncode}: "
        f"{completed.stderr.decode(errors='replace')}"
    )


def test_state_dir_relocates_kgl_store(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state_dir)])
    result = client.call_tool("kgl_query", {"query": "open_questions"})
    assert not result.get("isError"), result
    assert list(state_dir.glob("workspaces/*/kgl/kgl.db"))
    assert not (workspace / ".kgl").exists()


def test_state_dir_rejects_preexisting_kgl_database_symlink(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()
    kgl = state / "workspaces" / workspace_id / "kgl"
    kgl.mkdir(parents=True)
    outside = tmp_path / "outside.db"
    outside.write_bytes(b"")
    (kgl / "kgl.db").symlink_to(outside)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert outside.read_bytes() == b""
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_preexisting_kgl_database_hardlink(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.db"
    evidence.write_bytes(b"")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()
    kgl = state / "workspaces" / workspace_id / "kgl"
    kgl.mkdir(parents=True)
    os.link(evidence, kgl / "kgl.db")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert evidence.read_bytes() == b""
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_kgl_database_hardlink_to_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.db"
    connection = sqlite3.connect(evidence)
    connection.execute("CREATE TABLE original_data (value TEXT)")
    connection.commit()
    connection.close()
    original = evidence.read_bytes()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()
    kgl = state / "workspaces" / workspace_id / "kgl"
    kgl.mkdir(parents=True)
    proc = _spawn(daimonos_binary, workspace, state, tmp_path / "home")
    try:
        client = _handshake(proc, workspace)
        os.link(evidence, kgl / "kgl.db")
        result = client.call_tool("kgl_query", {"query": "open_questions"})
    finally:
        _terminate(proc)

    assert evidence.read_bytes() == original, "KGL changed a workspace file through a hard link"
    assert result.get("isError"), result


def test_state_dir_rejects_kgl_database_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()
    kgl = state / "workspaces" / workspace_id / "kgl"
    kgl.mkdir(parents=True)
    outside = tmp_path / "outside.db"
    outside.write_bytes(b"")
    proc = _spawn(daimonos_binary, workspace, state, tmp_path / "home")
    try:
        client = _handshake(proc, workspace)
        (kgl / "kgl.db").symlink_to(outside)
        result = client.call_tool("kgl_query", {"query": "open_questions"})
    finally:
        _terminate(proc)

    assert outside.read_bytes() == b"", "KGL wrote outside the selected state root"
    assert result.get("isError"), result


def test_state_dir_lists_kgl_tools_after_store_creation(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    result = client.call_tool("kgl_query", {"query": "open_questions"})
    assert not result.get("isError"), result

    names = {tool["name"] for tool in client.list_tools()}
    assert {"kgl_query", "kgl_assert"} <= names
    assert not (workspace / ".kgl").exists()


def test_state_dir_refuses_internal_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "daimonos.toml").write_text("[logging]\nenabled=true\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside"
    outside.mkdir()
    (state / "logs").symlink_to(outside, target_is_directory=True)
    result = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15,
    )
    assert result.returncode == 2, result.stderr.decode(errors="replace")
    assert list(outside.iterdir()) == []


def test_state_dir_refuses_socket_outside_selected_root(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside_socket = tmp_path / "outside.sock"
    result = subprocess.run(
        [daimonos_binary, "--state-dir", str(state), "-w", str(workspace),
         "--mcp-socket", str(outside_socket)],
        input=b"", capture_output=True, timeout=3,
    )
    assert result.returncode == 2, result.stderr.decode(errors="replace")
    assert not outside_socket.exists()


def _tool_text(result: dict) -> str:
    """Concatenate the text blocks of an MCP tool result."""
    return "".join(
        block.get("text", "")
        for block in result.get("content", [])
        if isinstance(block, dict)
    )


def test_state_dir_relocates_snapshot_store(daimonos_factory, tmp_path):
    """`snapshot create` (W03) must store copies under the selected root, never
    inside the workspace (evidence) tree."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "file.txt").write_text("content")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)

    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    result = client.call_tool("snapshot", {"action": "create", "tag": "v1"})
    assert not result.get("isError"), result

    manifests = list(state.glob("workspaces/*/snapshots/*/manifest.json"))
    assert manifests, f"expected a relocated snapshot under {state}, found none"
    assert not (workspace / ".daimonos").exists(), "snapshot leaked into the workspace"


def test_state_dir_snapshot_delete_rejects_parent_escape(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "file.txt").write_text("content")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside"
    outside.mkdir()
    sentinel = outside / "sentinel.txt"
    sentinel.write_text("keep")

    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    assert not client.call_tool("snapshot", {"action": "create"}).get("isError")
    snapshots = next(state.glob("workspaces/*/snapshots"))
    escaped_id = os.path.relpath(outside, snapshots)
    result = client.call_tool("snapshot", {"action": "delete", "id": escaped_id})

    assert result.get("isError") is True, result
    assert sentinel.read_text() == "keep"


def test_state_dir_snapshot_restore_rejects_symlink_escape(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "file.txt"
    evidence.write_text("original")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)

    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    created = client.call_tool("snapshot", {"action": "create"})
    snapshot_id = json.loads(_tool_text(created))["id"]
    snapshot = next(state.glob(f"workspaces/*/snapshots/{snapshot_id}"))
    outside = tmp_path / "outside"
    snapshot.rename(outside)
    (outside / "files" / "file.txt").write_text("outside")
    snapshot.symlink_to(outside, target_is_directory=True)
    evidence.write_text("current")

    result = client.call_tool("snapshot", {"action": "restore", "id": snapshot_id})

    assert result.get("isError") is True, result
    assert evidence.read_text() == "current"


def test_state_dir_snapshot_restore_rejects_symlinked_content(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "file.txt"
    evidence.write_text("original")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside.txt"
    outside.write_text("outside")

    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    created = client.call_tool("snapshot", {"action": "create"})
    snapshot_id = json.loads(_tool_text(created))["id"]
    saved = next(state.glob(f"workspaces/*/snapshots/{snapshot_id}/files/file.txt"))
    saved.unlink()
    saved.symlink_to(outside)
    evidence.write_text("current")

    result = client.call_tool("snapshot", {"action": "restore", "id": snapshot_id})

    assert result.get("isError") is True, result
    assert evidence.read_text() == "current"


def test_state_dir_snapshot_list_rejects_symlinked_manifest(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "file.txt").write_text("original")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)

    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    created = client.call_tool("snapshot", {"action": "create"})
    snapshot_id = json.loads(_tool_text(created))["id"]
    manifest = next(state.glob(f"workspaces/*/snapshots/{snapshot_id}/manifest.json"))
    outside = tmp_path / "outside.json"
    outside.write_text(manifest.read_text().replace('"tag": null', '"tag": "outside"'))
    manifest.unlink()
    manifest.symlink_to(outside)

    result = client.call_tool("snapshot", {"action": "list"})

    assert result.get("isError") is True, result
    assert "outside" not in _tool_text(result)


def test_state_dir_snapshot_rejects_absolute_ids(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "file.txt"
    evidence.write_text("original")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    created = client.call_tool("snapshot", {"action": "create"})
    snapshot_id = json.loads(_tool_text(created))["id"]
    snapshot = next(state.glob(f"workspaces/*/snapshots/{snapshot_id}"))
    outside = tmp_path / "outside"
    snapshot.rename(outside)
    evidence.write_text("current")

    for action in ("restore", "delete"):
        result = client.call_tool("snapshot", {"action": action, "id": str(outside)})
        assert result.get("isError") is True, result
    assert evidence.read_text() == "current"
    assert (outside / "manifest.json").is_file()


def test_state_dir_snapshot_lifecycle_accepts_generated_id(daimonos_factory, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "file.txt"
    evidence.write_text("original")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)

    client = daimonos_factory(workspace, args=["--state-dir", str(state)])
    created = client.call_tool("snapshot", {"action": "create"})
    snapshot_id = json.loads(_tool_text(created))["id"]
    evidence.write_text("changed")
    restored = client.call_tool("snapshot", {"action": "restore", "id": snapshot_id})
    assert not restored.get("isError"), restored
    assert evidence.read_text() == "original"

    deleted = client.call_tool("snapshot", {"action": "delete", "id": snapshot_id})
    assert not deleted.get("isError"), deleted
    assert not list(state.glob(f"workspaces/*/snapshots/{snapshot_id}"))


def test_state_dir_relocates_tool_output_spill(daimonos_factory, tmp_path):
    """A large tool result (W12) spills its full copy under the selected root's
    tool-output directory; the visible pointer references that state path and
    nothing is written under HOME."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "big.txt").write_text("x" * 300_000)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    home = tmp_path / "home"
    home.mkdir()

    client = daimonos_factory(
        workspace, env={"HOME": str(home)}, args=["--state-dir", str(state)]
    )
    result = client.call_tool("read_file", {"path": "big.txt"})
    text = _tool_text(result)
    assert "saved to" in text, f"output was not spilled: {text[:400]}"

    spill_dir = state / "tool-output"
    spills = list(spill_dir.glob("*.txt")) if spill_dir.exists() else []
    assert spills, f"expected a spill artifact under {spill_dir}, found none"
    # The pointer the agent sees must point into state, never outside it.
    assert str(spill_dir) in text, text[:400]
    assert not (home / ".daimonos").exists(), "tool output leaked into HOME"


def test_selected_mode_ignores_home_config_but_keeps_workspace(
    daimonos_binary, tmp_path
):
    """Explicit-state runs (D02) drop the HOME/XDG config candidate while the
    ordinary workspace `daimonos.toml` is still discovered. Legacy runs keep the
    HOME candidate, proving the suppression is scoped to the selected mode."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    (workspace / "daimonos.toml").write_text("[logging]\nenabled=true\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    home = tmp_path / "home"
    (home / ".config" / "daimonos").mkdir(parents=True)
    home_cfg = home / ".config" / "daimonos" / "config.toml"
    home_cfg.write_text("[logging]\nenabled=false\n")

    env = {**os.environ, "HOME": str(home)}
    env.pop("XDG_CONFIG_HOME", None)
    env.pop("XDG_DATA_HOME", None)

    selected = subprocess.run(
        [
            daimonos_binary,
            "--state-dir",
            str(state),
            "-w",
            str(workspace),
            "--print-config-path",
        ],
        capture_output=True,
        text=True,
        timeout=15,
        env=env,
    )
    assert selected.returncode == 0, selected.stderr
    assert str(home_cfg) not in selected.stdout, selected.stdout
    assert str(workspace / "daimonos.toml") in selected.stdout, selected.stdout

    legacy = subprocess.run(
        [daimonos_binary, "-w", str(workspace), "--print-config-path"],
        capture_output=True,
        text=True,
        timeout=15,
        env=env,
    )
    assert legacy.returncode == 0, legacy.stderr
    assert str(home_cfg) in legacy.stdout, legacy.stdout


def test_explicit_config_is_honored_in_selected_mode(daimonos_binary, tmp_path):
    """An explicit `--config` is a trusted control input: it stays first in the
    discovery order even in selected-state mode (ADR-021)."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    explicit = tmp_path / "explicit.toml"
    explicit.write_text("[logging]\nenabled=true\n")
    home = _unusable_home(tmp_path)

    env = {**os.environ, "HOME": str(home)}
    completed = subprocess.run(
        [
            daimonos_binary,
            "--state-dir",
            str(state),
            "-w",
            str(workspace),
            "--config",
            str(explicit),
            "--print-config-path",
        ],
        capture_output=True,
        text=True,
        timeout=15,
        env=env,
    )
    assert completed.returncode == 0, completed.stderr
    assert str(explicit) in completed.stdout, completed.stdout


def test_unwritable_state_root_refused_without_fallback(daimonos_binary, tmp_path):
    """A selected root that exists but is not writable is an init failure for
    every enabled writer. The run must refuse (exit 2) rather than silently fall
    back to HOME state (slice 4 / ADR-021)."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o500)  # owner r-x only: not writable, still private
    home = _unusable_home(tmp_path)

    env = {**os.environ, "HOME": str(home)}
    try:
        completed = subprocess.run(
            [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
            input=b"",
            capture_output=True,
            timeout=15,
            env=env,
        )
        assert completed.returncode == 2, (
            f"expected exit 2 for unwritable state root, got "
            f"{completed.returncode}: {completed.stderr.decode(errors='replace')}"
        )
        assert not (home / ".daimonos").exists(), "state fell back into HOME"
    finally:
        os.chmod(state, 0o700)


def test_failed_state_option_leaves_workspace_unmutated(daimonos_binary, tmp_path):
    """A rejected state option (here an internal socket outside the selected
    root) must fail before any writer and leave the workspace (evidence) tree
    untouched (slice 5)."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "marker.txt").write_text("evidence")
    before = sorted(p.name for p in workspace.iterdir())
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside_socket = tmp_path / "outside.sock"

    result = subprocess.run(
        [
            daimonos_binary,
            "--state-dir",
            str(state),
            "-w",
            str(workspace),
            "--mcp-socket",
            str(outside_socket),
        ],
        input=b"",
        capture_output=True,
        timeout=5,
    )
    assert result.returncode == 2, result.stderr.decode(errors="replace")
    assert not outside_socket.exists()
    after = sorted(p.name for p in workspace.iterdir())
    assert before == after, f"workspace was mutated by a failed run: {after}"


def test_state_dir_rejects_explicit_legacy_default_socket_outside_root(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside_socket = "/tmp/daimonos.sock"
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--socket", outside_socket],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "--socket" in error["message"]
    assert outside_socket in error["message"]


def test_state_dir_accepts_explicit_nested_socket_inside_root(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    socket_path = state / "custom" / "mcp.sock"
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp-socket", str(socket_path), "--state-dir", str(state),
         "-w", str(workspace)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env,
    )
    try:
        import time
        deadline = time.monotonic() + 10
        while not socket_path.exists() and proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        assert proc.poll() is None, proc.stderr.read().decode(errors="replace")
        assert stat.S_ISSOCK(socket_path.stat().st_mode)
    finally:
        if proc.poll() is None:
            proc.terminate()
        proc.wait(timeout=5)


@pytest.mark.parametrize("workspace_kind", ["missing", "file"])
def test_state_dir_rejects_invalid_workspace_with_structured_error(daimonos_binary, tmp_path, workspace_kind):
    workspace = tmp_path / "evidence"
    if workspace_kind == "file":
        workspace.write_text("not a directory")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert str(workspace) in error["message"]
    assert not list(state.iterdir())


def test_state_dir_rejects_unimplemented_mcp_config_before_writing(daimonos_binary, tmp_path):
    """Unsupported modes must not silently ignore the selected state root."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_DATA_HOME=str(home / ".local" / "share"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--state-dir", str(state), "-w", str(workspace),
         "mcp-config", "sync", "--dry-run"],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_mode_unsupported"
    assert not list(home.rglob("*")), "unsupported MCP config mode wrote under HOME"


def test_state_dir_rejects_explicit_log_destination_outside_root(daimonos_binary, tmp_path):
    """Do not silently replace a caller's invalid internal destination."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside-logs"
    config = tmp_path / "explicit.toml"
    config.write_text("[logging]\nenabled = true\ndirectory = " + json.dumps(str(outside)) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config), "--debug-tokens"],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert "logging.directory" in error["message"]
    assert str(outside) in error["message"]
    assert not outside.exists()
    assert not list(state.iterdir()), "debug-token state was created before validation"


def test_state_dir_honors_explicit_log_destination_inside_root(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    custom_logs = state / "custom-logs"
    config = tmp_path / "explicit.toml"
    config.write_text("[logging]\nenabled = true\ndirectory = " + json.dumps(str(custom_logs)) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 0, completed.stderr.decode(errors="replace")
    assert custom_logs.is_dir()
    assert list(custom_logs.iterdir()), "configured log destination was silently replaced"


def test_state_dir_rejects_log_path_through_escaping_symlink(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside"
    outside.mkdir()
    (state / "link").symlink_to(outside, target_is_directory=True)
    redirected = state / "link" / "logs"
    config = tmp_path / "explicit.toml"
    config.write_text("[logging]\ndirectory = " + json.dumps(str(redirected)) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert "logging.directory" in error["message"]
    assert str(redirected) in error["message"]
    assert not (outside / "logs").exists()


@pytest.mark.parametrize(("section", "field"), [
    ("analytics", "db_path"),
    ("coordination", "db_dir"),
    ("tool_output", "directory"),
    ("process", "artifact_directory"),
    ("git_read", "status_directory"),
    ("session", "socket_path"),
    ("session", "session_catalog_path"),
])
def test_state_dir_rejects_all_outside_internal_overrides(daimonos_binary, tmp_path, section, field):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside"
    config = tmp_path / "explicit.toml"
    config.write_text(f"[{section}]\n{field} = " + json.dumps(str(outside)) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert f"{section}.{field}" in error["message"]
    assert str(outside) in error["message"]
    assert not outside.exists()


def test_state_dir_fails_when_enabled_logging_cannot_initialize(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    logs = state / "logs"
    logs.mkdir()
    (logs / "daimonos").mkdir()  # The non-rotating log file cannot be a directory.
    config = tmp_path / "logging.toml"
    config.write_text('[logging]\nrotation = "never"\n')
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "logging.directory" in error["message"]


def test_state_dir_rejects_log_file_hardlink_to_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.txt"
    evidence.write_text("unchanged")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    logs = state / "logs"
    logs.mkdir()
    os.link(evidence, logs / "daimonos")
    config = tmp_path / "logging.toml"
    config.write_text('[logging]\nrotation = "never"\n')
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert evidence.read_text() == "unchanged", "logging changed a workspace file through a hard link"
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_log_file_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    logs = state / "logs"
    logs.mkdir()
    outside = tmp_path / "outside.log"
    outside.write_text("unchanged")
    (logs / "daimonos").symlink_to(outside)
    config = tmp_path / "logging.toml"
    config.write_text('[logging]\nrotation = "never"\n')
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace),
         "--config", str(config)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert outside.read_text() == "unchanged", "logging wrote outside the selected state root"
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_analytics_wal_hardlink_to_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.txt"
    evidence.write_bytes(b"unchanged")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    analytics = state / "analytics"
    analytics.mkdir()
    connection = sqlite3.connect(analytics / "analytics.db")
    connection.execute("CREATE TABLE original_data (value TEXT)")
    connection.commit()
    connection.close()
    os.link(evidence, analytics / "analytics.db-wal")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert evidence.read_bytes() == b"unchanged", "analytics changed a workspace file through a WAL hard link"
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_analytics_database_hardlink_to_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    evidence = workspace / "evidence.db"
    connection = sqlite3.connect(evidence)
    connection.execute("CREATE TABLE original_data (value TEXT)")
    connection.commit()
    connection.close()
    original = evidence.read_bytes()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    analytics = state / "analytics"
    analytics.mkdir()
    os.link(evidence, analytics / "analytics.db")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert evidence.read_bytes() == original, "analytics changed a workspace file through a hard link"
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_rejects_analytics_database_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    analytics = state / "analytics"
    analytics.mkdir()
    outside = tmp_path / "outside.db"
    outside.write_bytes(b"")
    (analytics / "analytics.db").symlink_to(outside)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert outside.read_bytes() == b"", "analytics wrote outside the selected state root"
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"


def test_state_dir_fails_when_enabled_analytics_cannot_initialize(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    analytics = state / "analytics"
    analytics.mkdir()
    (analytics / "analytics.db").mkdir()  # SQLite cannot open a directory as its database.
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "analytics.db_path" in error["message"]


def test_state_dir_rejects_file_in_reserved_kgl_directory(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    workspace_id = hashlib.sha256(os.fsencode(workspace.resolve())).hexdigest()
    namespace = state / "workspaces" / workspace_id
    namespace.mkdir(parents=True)
    (namespace / "kgl").write_text("not a directory")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
        input=b"", capture_output=True, timeout=15, env=env,
    )
    assert completed.returncode == 2, completed.stderr.decode(errors="replace")
    error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert "kgl" in error["message"]


@pytest.mark.skipif(os.geteuid() == 0, reason="root bypasses directory permissions")
@pytest.mark.parametrize("component", ["tool-output", "coordination"])
def test_state_dir_rejects_unwritable_enabled_writer_directory(daimonos_binary, tmp_path, component):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    blocked = state / component
    blocked.mkdir()
    os.chmod(blocked, 0o000)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    try:
        completed = subprocess.run(
            [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
            input=b"", capture_output=True, timeout=15, env=env,
        )
        assert completed.returncode == 2, completed.stderr.decode(errors="replace")
        error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
        assert error["code"] == "run_configuration_invalid"
        assert error["reason"] == "state_unusable"
        assert component in error["message"]
    finally:
        os.chmod(blocked, 0o700)


@pytest.mark.skipif(os.geteuid() == 0, reason="root bypasses directory permissions")
def test_state_dir_rejects_uninspectable_layout(daimonos_binary, tmp_path):
    """An inaccessible component is not the same as a missing component."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    blocked = state / "workspaces"
    blocked.mkdir(mode=0o000)
    os.chmod(blocked, 0o000)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(tmp_path / "home"), XDG_CONFIG_HOME=str(tmp_path / "xdg"),
               XDG_DATA_HOME=str(tmp_path / "data"), TMPDIR=str(tmp_path))
    try:
        completed = subprocess.run(
            [daimonos_binary, "--mcp", "--state-dir", str(state), "-w", str(workspace)],
            input=b"", capture_output=True, timeout=15, env=env,
        )
        assert completed.returncode == 2, completed.stderr.decode(errors="replace")
        error = json.loads(completed.stderr.decode().splitlines()[-1])["error"]
        assert error["code"] == "run_configuration_invalid"
        assert error["reason"] == "state_unusable"
    finally:
        os.chmod(blocked, 0o700)
