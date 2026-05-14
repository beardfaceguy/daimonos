"""Read 100 small files sequentially.

Exercises the file-IO opcode path: dispatch overhead, read_cache
fast-path on repeat reads, and structured response serialization. The
files are 256 bytes so per-call I/O is negligible — the timing reflects
opcode dispatch + cache lookup + JSON framing, not disk throughput.
"""

from __future__ import annotations

import time
from pathlib import Path

ID = "read_100"
DESCRIPTION = "Read 100 small (256 B) files sequentially"

FILE_COUNT = 100
PAYLOAD = "x" * 256


def setup(workspace: Path) -> None:
    for i in range(FILE_COUNT):
        (workspace / f"f{i:03d}.txt").write_text(PAYLOAD)


def run_iteration(client, workspace: Path):
    samples: list[tuple[int, int]] = []
    for i in range(FILE_COUNT):
        op = {"c": 0, "p": str(workspace / f"f{i:03d}.txt")}
        start = time.perf_counter_ns()
        resp = client.call(op)
        elapsed = time.perf_counter_ns() - start
        if not resp.get("ok", False):
            raise RuntimeError(f"read_100 failed at i={i}: {resp.get('m')!r}")
        samples.append((0, elapsed))
    return samples
