"""WP13 / FR-11 (ADR-021): no-spill bounded tool output.

The caller-owned ``--no-output-spill`` option bounds oversized tool results in
place: the model-visible text still says it was truncated and how to get the
rest (paging / query narrowing), but no full-output copy is written and no
saved-file pointer is emitted. The reviewer runs with a read-only state root it
cannot open, so a pointer to a spill file would only mislead it.

Default behavior (a written spill file plus a ``full output saved to`` pointer)
is exercised by ``tests/test_read_write.py`` and ``tests/test_exec.py``; the
opt-out regression here spawns the same build without the flag and confirms the
pointer returns.
"""

from __future__ import annotations

from pathlib import Path


def _write_bound_config(workspace: Path, spill_dir: Path) -> None:
    """Lower the output cap and pin the spill directory into the test tree.

    A 256-byte cap (the minimum ``tool_output.validate`` allows) makes any real
    read exceed it, and pinning ``directory`` lets the test assert the spill
    directory stays empty instead of reaching into the real HOME.
    """
    (workspace / "daimonos.toml").write_text(
        "[tool_output]\n"
        f'directory = "{spill_dir.as_posix()}"\n'
        "max_bytes = 256\n"
        "max_lines = 6\n"
    )


def test_no_spill_read_is_truncated_with_paging_and_no_file(daimonos_factory, tmp_path):
    workspace = tmp_path / "ws"
    workspace.mkdir()
    spill_dir = tmp_path / "spill"
    _write_bound_config(workspace, spill_dir)

    # Comfortably over the 256-byte cap so the result must be bounded.
    (workspace / "big.txt").write_text("sentinel-line\n" + ("x" * 5_000))

    client = daimonos_factory(workspace, args=["--no-output-spill"])
    result = client.call_tool("read_file", {"path": "big.txt"})
    visible = result["content"][0]["text"]

    # Explicit truncation is preserved — never claim a partial result is whole.
    assert "truncated" in visible, visible
    # Paging / narrowing guidance replaces the file pointer.
    assert "offset" in visible and "limit" in visible, visible
    # No saved-file pointer, and no leak of the (inaccessible) spill path.
    assert "saved to" not in visible, visible
    assert spill_dir.as_posix() not in visible, visible
    # No spill file was produced at the producer.
    assert not spill_dir.exists() or not any(spill_dir.iterdir()), list(
        spill_dir.iterdir()
    )


def test_no_spill_process_output_is_truncated_without_a_file(daimonos_factory, tmp_path):
    """AC-11.1 across the process route: exec output over the cap is bounded in
    place with no spill file and no pointer, exactly as for native reads."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    spill_dir = tmp_path / "spill"
    _write_bound_config(workspace, spill_dir)

    client = daimonos_factory(workspace, args=["--no-output-spill"])
    result = client.call_tool(
        "exec", {"command": "sh", "args": ["-c", "yes x | head -n 6000"]}
    )
    visible = result["content"][0]["text"]

    assert "truncated" in visible, visible
    assert "offset" in visible and "limit" in visible, visible
    assert "saved to" not in visible, visible
    assert spill_dir.as_posix() not in visible, visible
    assert not spill_dir.exists() or not any(spill_dir.iterdir()), list(
        spill_dir.iterdir()
    )


def test_opt_out_default_still_spills_and_points_at_a_file(daimonos_factory, tmp_path):
    """Regression: with the flag absent, the existing offload behavior (a spill
    file plus a ``full output saved to`` pointer) is preserved unchanged."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    spill_dir = tmp_path / "spill"
    _write_bound_config(workspace, spill_dir)
    (workspace / "big.txt").write_text("sentinel-line\n" + ("x" * 5_000))

    client = daimonos_factory(workspace)  # no --no-output-spill
    result = client.call_tool("read_file", {"path": "big.txt"})
    visible = result["content"][0]["text"]

    assert "full output saved to" in visible, visible
    assert spill_dir.as_posix() in visible, visible
    assert spill_dir.exists() and any(spill_dir.iterdir()), "default mode spills a file"
