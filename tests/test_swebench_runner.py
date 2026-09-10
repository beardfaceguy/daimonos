"""Tests for the Daimonos SWE-bench runner."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path


RUNNER = (
    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "run_agent.py"
)


def load_runner():
    spec = importlib.util.spec_from_file_location("swebench_run_agent", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_run_text_replaces_non_utf8_subprocess_output():
    runner = load_runner()

    completed = runner.run_text(
        [sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'\\xb6')"]
    )

    assert completed.returncode == 0
    assert completed.stdout == "\ufffd"


def test_collect_patch_encodes_binary_files(tmp_path):
    runner = load_runner()
    subprocess.run(["git", "init", "-q", str(tmp_path)], check=True)
    subprocess.run(
        ["git", "-C", str(tmp_path), "config", "user.email", "test@example.com"],
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(tmp_path), "config", "user.name", "Test"],
        check=True,
    )
    tracked = tmp_path / "tracked.txt"
    tracked.write_text("tracked\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "tracked.txt"], check=True)
    subprocess.run(
        ["git", "-C", str(tmp_path), "commit", "-qm", "fixture"],
        check=True,
    )
    (tmp_path / "artifact.bin").write_bytes(b"\x00\xb6")

    patch = runner.collect_patch(tmp_path)

    assert "GIT binary patch" in patch
    assert "\ufffd" not in patch
