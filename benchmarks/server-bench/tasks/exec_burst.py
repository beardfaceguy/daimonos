"""Run 30 cheap exec calls (`true`) to stress process-spawn overhead.

Why `true` and not `echo`: `true` produces no stdout/stderr, so timing
isolates fork + exec + waitpid + response framing without any noise
from exec_filter inspecting output or stdout/stderr being captured.

Why not a plugin-redirected command like `git status`: this would hit
the plugin layer too, conflating two cost centers. exec_burst is for
the bare exec path; plugin-redirect cost belongs in a separate
scenario if and when we add one.
"""

from __future__ import annotations

import time
from pathlib import Path

ID = "exec_burst"
DESCRIPTION = "Run `true` 30 times via exec opcode"

BURST_COUNT = 30


def setup(workspace: Path) -> None:
    _ = workspace  # nothing to set up


def run_iteration(client, workspace: Path):
    samples: list[tuple[int, int]] = []
    cwd = str(workspace)
    for _ in range(BURST_COUNT):
        op = {"c": 8, "s": "true", "q": cwd}
        start = time.perf_counter_ns()
        resp = client.call(op)
        elapsed = time.perf_counter_ns() - start
        if not resp.get("ok", False):
            raise RuntimeError(f"exec_burst failed: {resp.get('m')!r}")
        samples.append((8, elapsed))
    return samples
