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


def test_no_spill_background_process_bounds_in_place_without_a_file(
    daimonos_binary, tmp_path
):
    """R1 (ADR-021, FR-11): the background/managed-process capture path honors
    no-spill too.

    A background child whose output exceeds the artifact cap under
    ``--no-output-spill`` must be bounded *in memory*: no artifact file is
    written in any state location and no artifact path is advertised, while the
    (bounded) output stays readable through ``poll``. Background exec is only
    reachable over the native op socket (``bg`` = opcode 9, ``poll`` = 10), so
    this drives an unconfined ``--socket`` daemon the way
    ``tests/test_path_confinement.py`` does.
    """
    import json
    import os
    import socket
    import subprocess
    import tempfile
    import time

    workspace = tmp_path / "ws"
    workspace.mkdir()
    # A private HOME keeps any default state location inside the test tree.
    home = tmp_path / "home"
    home.mkdir()

    # A short directory keeps the socket path under the Unix length limit. The
    # state root must contain the process-artifact directory, so pin the
    # artifact dir *inside* it and lower the cap so the child's output exceeds
    # it; under no-spill nothing must be written there.
    with tempfile.TemporaryDirectory(prefix="dn-", dir="/tmp") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        artifact_dir = state / "pstate"
        (workspace / "daimonos.toml").write_text(
            "[process]\n"
            f'artifact_directory = "{artifact_dir.as_posix()}"\n'
            "artifact_max_bytes = 256\n"
            "poll_tail_lines = 20\n"
        )
        socket_path = state / "native.sock"
        env = dict(os.environ)
        env["HOME"] = str(home)
        proc = subprocess.Popen(
            [
                daimonos_binary,
                "--workspace",
                str(workspace),
                "--state-dir",
                str(state),
                "--socket",
                str(socket_path),
                "--no-output-spill",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        start = poll = None
        try:
            deadline = time.monotonic() + 15
            while not socket_path.exists() and time.monotonic() < deadline:
                assert proc.poll() is None, proc.stderr.read().decode(errors="replace")
                time.sleep(0.01)
            assert socket_path.exists(), "daemon native socket never appeared"

            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(socket_path))
                with client.makefile("rwb") as stream:

                    def send(req):
                        stream.write((json.dumps(req) + "\n").encode())
                        stream.flush()
                        return json.loads(stream.readline())

                    start = send(
                        {
                            "c": 9,
                            "s": "sh",
                            "a": ["-c", "yes 0123456789abcdef | head -n 4000"],
                            "q": str(workspace),
                        }
                    )
                    assert start["ok"] is True, start
                    # No saved-file pointer may be advertised under no-spill.
                    assert "log" not in start["d"], start
                    pid = start["d"]["pid"]

                    tail = None
                    for _ in range(200):
                        poll = send({"c": 10, "n": pid})
                        assert poll["ok"] is True, poll
                        tail = poll["d"].get("tail") or tail
                        if poll["d"]["running"] is False:
                            break
                        time.sleep(0.02)
                    else:
                        raise AssertionError("bg child never reported exit")

                    # The bounded output is still readable in place...
                    assert tail, f"bounded output must still be readable: {poll}"
                    # ...and no response advertises a saved-file path.
                    blob = json.dumps({"start": start, "poll": poll})
                    assert artifact_dir.as_posix() not in blob, blob
                    assert "log" not in poll["d"], poll
        finally:
            if proc.poll() is None:
                proc.terminate()
            proc.communicate(timeout=5)

        # No artifact file was written in the pinned (state-dir) location...
        assert not artifact_dir.exists() or not any(artifact_dir.iterdir()), list(
            artifact_dir.iterdir()
        )
        # ...nor in the default HOME-based process-output location.
        default_dir = home / ".daimonos" / "process-output"
        assert not default_dir.exists() or not any(default_dir.iterdir()), list(
            default_dir.iterdir()
        )


def _state_inventory(root: Path) -> set[str]:
    """Every regular file under ``root``, as root-relative POSIX paths."""
    if not root.exists():
        return set()
    return {
        p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file()
    }


def test_state_file_inventory_is_unchanged_under_no_spill(daimonos_factory, tmp_path):
    """Plan checkpoint: with the option on, producing oversized native *and*
    process output adds no file to the state inventory.

    W12 (`tool_output::store_full`) and W27 (`managed_process::create_artifact`)
    are the only state locations a bounded tool result would write to, and both
    are forbidden under no-spill. So an isolated HOME (where the default
    `~/.daimonos/tool-output` and `~/.daimonos/process-output` resolve) must
    hold exactly the same files before and after the oversized reads. The
    opt-out leg runs the identical workload *without* the flag and shows a spill
    file does appear — proving the inventory method would catch a regression.
    """
    import os

    def run(home: Path, args) -> tuple[set[str], set[str], str]:
        workspace = tmp_path / f"ws-{home.name}"
        workspace.mkdir()
        _write_bound_config(workspace, home / ".daimonos" / "tool-output")
        (workspace / "big.txt").write_text("sentinel-line\n" + ("x" * 5_000))
        client = daimonos_factory(workspace, env={"HOME": str(home)}, args=args)
        before = _state_inventory(home)
        read = client.call_tool("read_file", {"path": "big.txt"})
        execd = client.call_tool(
            "exec", {"command": "sh", "args": ["-c", "yes x | head -n 6000"]}
        )
        after = _state_inventory(home)
        visible = read["content"][0]["text"] + "\n" + execd["content"][0]["text"]
        return before, after, visible

    # --- no-spill on: the inventory must not grow ---
    home_on = tmp_path / "home-on"
    (home_on / ".daimonos").mkdir(parents=True)
    before_on, after_on, visible_on = run(home_on, ["--no-output-spill"])
    assert after_on == before_on, (
        "no-spill produced oversized native+process output but changed the "
        f"state inventory: added {sorted(after_on - before_on)}, "
        f"removed {sorted(before_on - after_on)}"
    )
    # And no saved-file pointer was advertised on either route.
    assert "saved to" not in visible_on, visible_on

    # --- opt-out contrast: the same workload *does* spill a file ---
    home_off = tmp_path / "home-off"
    (home_off / ".daimonos").mkdir(parents=True)
    before_off, after_off, visible_off = run(home_off, None)
    added_off = after_off - before_off
    assert added_off, "default mode must spill at least one file (inventory sanity)"
    assert any("tool-output" in p for p in added_off), added_off
    assert "full output saved to" in visible_off, visible_off
