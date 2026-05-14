"""CI smoke test for the deterministic server-bench harness.

Runs a single task at tiny replicate count to catch breakage of the
bench wiring itself: socket connect, opcode round-trip, JSON
serialization, output schema. Not a perf assertion — there's no
threshold check here on purpose, because CI machines vary too much for
absolute timings to be meaningful. The point is "does the harness
still produce well-formed output end-to-end."

If you're hunting for a regression in actual daimonos performance, run
`bench.py` directly with --replicates 20+ on consistent hardware and
diff against a known-good `results.json` via `compare.py`.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
BENCH = REPO_ROOT / "benchmarks" / "server-bench" / "bench.py"


@pytest.mark.skipif(
    not (REPO_ROOT / "target" / "release" / "daimonos").exists()
    and not (REPO_ROOT / "target" / "debug" / "daimonos").exists(),
    reason="no daimonos binary; build first",
)
def test_bench_harness_runs_one_task(tmp_path):
    """End-to-end: bench.py spawns daimonos, runs read_100 × 2 replicates,
    writes a parseable results.json. Anything along the chain breaking
    surfaces here."""
    out = tmp_path / "results"
    proc = subprocess.run(
        [
            sys.executable,
            str(BENCH),
            "--tasks",
            "read_100",
            "--replicates",
            "2",
            "--out",
            str(out),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert proc.returncode == 0, f"bench.py failed: stderr={proc.stderr!r}"

    results_file = out / "results.json"
    assert results_file.exists(), f"missing {results_file}"
    payload = json.loads(results_file.read_text())

    assert payload["replicates"] == 2
    assert len(payload["tasks"]) == 1
    task = payload["tasks"][0]
    assert task["task_id"] == "read_100"
    # 100 reads per replicate × 2 replicates = 200 timings.
    assert len(task["timings_ns"]) == 200
    assert len(task["op_codes"]) == 200
    # All ops should be opcode 0 (read).
    assert set(task["op_codes"]) == {0}
    # Summary block well-formed.
    s = task["summary"]
    assert s["count"] == 200
    assert s["median_ns"] > 0
    assert s["p99_ns"] >= s["median_ns"]
    assert s["max_ns"] >= s["p99_ns"]
