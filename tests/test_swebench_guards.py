"""Tests for SWE-bench guard calibration reports."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "benchmarks"
    / "swebench"
    / "analyze_guards.py"
)


def test_guard_analysis_reports_correctness_tradeoff(tmp_path):
    (tmp_path / "a.json").write_text(
        json.dumps(
            {
                "task_id": "a",
                "cost_usd": 2.5,
                "wall_ms": 250_000,
            }
        )
    )
    (tmp_path / "b.json").write_text(
        json.dumps(
            {
                "task_id": "b",
                "cost_usd": 1.0,
                "wall_ms": 350_000,
            }
        )
    )
    evaluator = tmp_path / "evaluator.json"
    evaluator.write_text(
        json.dumps({"resolved_ids": ["a"], "unresolved_ids": ["b"]})
    )
    (tmp_path / "guard-summary.json").write_text(
        json.dumps(
            {
                "task_id": "not-an-instance-summary",
                "cost_usd": 99,
                "wall_ms": 999_000,
            }
        )
    )

    completed = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--evaluator-report",
            str(evaluator),
            str(tmp_path),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )

    report = json.loads(completed.stdout)
    assert report["instances"] == 2
    assert report["observed_cost_usd"] == 3.5
    assert report["would_trigger"] == ["a", "b"]
    assert report["correct_would_trigger"] == ["a"]
    assert report["incorrect_would_trigger"] == ["b"]


def test_guard_analysis_rejects_conflicting_correctness(tmp_path):
    (tmp_path / "a.json").write_text(
        json.dumps(
            {
                "task_id": "a",
                "cost_usd": 2.5,
                "wall_ms": 250_000,
                "correct": False,
            }
        )
    )
    evaluator = tmp_path / "evaluator.json"
    evaluator.write_text(json.dumps({"resolved_ids": ["a"]}))

    completed = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--evaluator-report",
            str(evaluator),
            str(tmp_path),
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert completed.returncode == 2
    assert "correctness conflict for a" in completed.stderr
