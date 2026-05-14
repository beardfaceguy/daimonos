"""Snapshot create + restore + delete cycle, repeated N times per iteration.

Stresses the async-fs path that landed in vikunja #252 (snapshot.rs moved
off the runtime via tokio::fs + spawn_blocking for remove_dir_all). Each
cycle creates a fresh snapshot of a small workspace, restores it, then
deletes it — so any regression in snapshot manifest serialization,
recursive copy, or remove_dir_all stands out clearly.

Each iteration runs CYCLES cycles to amortize the constant-ish workspace
setup; per-call timings still come out individually.
"""

from __future__ import annotations

import time
from pathlib import Path

ID = "snapshot_cycle"
DESCRIPTION = "Snapshot create/restore/delete cycle × 5 per iteration"

# 5 cycles per iteration × default 20 iterations = 100 of each op
# overall. Larger numbers stretch each iteration into multiple seconds,
# which is OK for `bench.py` but bad for the CI smoke test, so we keep
# this conservative.
CYCLES_PER_ITERATION = 5

# Realistic-ish content layout. Total payload ~16 KB across 8 files —
# small enough to be quick but big enough to exercise recursive
# directory copy/delete.
FILES = {
    "src/main.rs": "fn main() {\n    println!(\"hello\");\n}\n" * 50,
    "src/lib.rs": "pub mod inner;\n" * 50,
    "src/inner/mod.rs": "// inner module\n" * 50,
    "src/inner/util.rs": "pub fn util() {}\n" * 50,
    "tests/basic.rs": "#[test] fn it_works() {}\n" * 50,
    "Cargo.toml": '[package]\nname = "snap-bench"\nversion = "0.1.0"\n',
    "README.md": "# snap-bench\n" * 50,
    ".gitignore": "/target\n",
}


def setup(workspace: Path) -> None:
    for rel, content in FILES.items():
        target = workspace / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)


def run_iteration(client, workspace: Path):
    samples: list[tuple[int, int]] = []
    for _ in range(CYCLES_PER_ITERATION):
        start = time.perf_counter_ns()
        resp = client.call({"c": 12})
        elapsed_create = time.perf_counter_ns() - start
        if not resp.get("ok", False):
            raise RuntimeError(f"snap create failed: {resp.get('m')!r}")
        samples.append((12, elapsed_create))
        meta = resp.get("d") or {}
        snap_id = meta.get("id")
        if not snap_id:
            raise RuntimeError(f"snap create returned no id: {meta!r}")

        start = time.perf_counter_ns()
        resp = client.call({"c": 13, "p": snap_id})
        elapsed_restore = time.perf_counter_ns() - start
        if not resp.get("ok", False):
            raise RuntimeError(f"snap restore failed: {resp.get('m')!r}")
        samples.append((13, elapsed_restore))

        start = time.perf_counter_ns()
        resp = client.call({"c": 26, "p": snap_id})
        elapsed_delete = time.perf_counter_ns() - start
        if not resp.get("ok", False):
            raise RuntimeError(f"snap delete failed: {resp.get('m')!r}")
        samples.append((26, elapsed_delete))
    return samples
