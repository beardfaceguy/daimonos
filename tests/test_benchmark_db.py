import json
import sqlite3
import subprocess
import sys
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "benchmarks" / "benchmark_db.py"


def write_run(results, run_id, total_tokens, cost):
    run_dir = results / run_id
    run_dir.mkdir(parents=True)
    (run_dir / "task.json").write_text(json.dumps({
        "task_id": "task",
        "task_name": "Task",
        "started_at": "2026-01-01T00:00:00Z",
        "ended_at": "2026-01-01T00:00:01Z",
        "wall_ms": 1000,
        "input": total_tokens - 10,
        "cache_write": 0,
        "cache_read": 0,
        "output": 10,
        "prompt_tokens": total_tokens - 10,
        "total_tokens": total_tokens,
        "cost_usd": cost,
        "llm_calls": 2,
        "correct": True,
        "checks_passed": 1,
        "checks_total": 1,
        "context_component_tokens_est_total": {"tool_schema_bytes": 20},
    }))


def test_sync_is_idempotent_and_compare_enforces_scope(tmp_path):
    results = tmp_path / "results"
    write_run(results, "baseline-run", 100, 1.0)
    write_run(results, "candidate-run", 80, 0.7)
    manifest = tmp_path / "lineage.json"
    manifest.write_text(json.dumps({
        "schema_version": 1,
        "stages": [
            {
                "id": "B0",
                "parent": None,
                "feature": "baseline",
                "scope_fingerprint": "suite-v1",
                "benchmark_kind": "full_suite",
                "provider": "anthropic",
                "model": "model",
                "thinking": "medium",
                "task_ids": ["task"],
                "binary_sha256": "base",
                "run_dirs": ["baseline-run"],
                "metrics": {"mean_total_tokens": [100, "tokens"]},
            },
            {
                "id": "B1",
                "parent": "B0",
                "feature": "optimization",
                "scope_fingerprint": "suite-v1",
                "benchmark_kind": "full_suite",
                "provider": "anthropic",
                "model": "model",
                "thinking": "medium",
                "task_ids": ["task"],
                "binary_sha256": "candidate",
                "run_dirs": ["candidate-run"],
                "metrics": {"mean_total_tokens": [80, "tokens"]},
            },
            {
                "id": "X0",
                "parent": None,
                "feature": "other scope",
                "scope_fingerprint": "other-v1",
                "benchmark_kind": "targeted",
                "provider": "anthropic",
                "model": "model",
                "thinking": "medium",
                "task_ids": ["task"],
                "binary_sha256": "other",
                "run_dirs": [],
                "metrics": {},
            },
        ],
    }))
    db = tmp_path / "private" / "benchmarks.db"

    sync = [
        sys.executable,
        str(SCRIPT),
        "--db",
        str(db),
        "sync",
        "--manifest",
        str(manifest),
        "--results-dir",
        str(results),
    ]
    subprocess.run(sync, check=True, capture_output=True, text=True)
    subprocess.run(sync, check=True, capture_output=True, text=True)

    assert oct(db.stat().st_mode & 0o777) == "0o600"
    with sqlite3.connect(db) as connection:
        assert connection.execute("SELECT COUNT(*) FROM benchmark_stages").fetchone()[0] == 3
        assert connection.execute("SELECT COUNT(*) FROM benchmark_runs").fetchone()[0] == 2
        assert connection.execute("SELECT COUNT(*) FROM stage_runs").fetchone()[0] == 2
        assert connection.execute("SELECT COUNT(*) FROM task_results").fetchone()[0] == 2

    compared = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--db",
            str(db),
            "compare",
            "B0",
            "B1",
            "--json",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    comparison = json.loads(compared.stdout)
    assert comparison["total_tokens"]["baseline"] == 100
    assert comparison["total_tokens"]["candidate"] == 80
    assert comparison["total_tokens"]["delta_pct"] == -20
    assert comparison["per_task"]["task"]["delta_pct"] == -20
    assert comparison["per_task"]["task"]["cost_usd"]["delta_pct"] == -30
    assert comparison["per_task"]["task"]["wall_ms"]["delta_pct"] == 0

    mismatch = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--db",
            str(db),
            "compare",
            "B0",
            "X0",
        ],
        capture_output=True,
        text=True,
    )
    assert mismatch.returncode != 0
    assert "scope fingerprints differ" in mismatch.stderr

    candidate_summary = results / "candidate-run" / "task.json"
    original_summary = candidate_summary.read_text()
    changed_summary = json.loads(original_summary)
    changed_summary["total_tokens"] = 81
    candidate_summary.write_text(json.dumps(changed_summary))
    immutable_run = subprocess.run(sync, capture_output=True, text=True)
    assert immutable_run.returncode != 0
    assert "run candidate-run is immutable" in immutable_run.stderr
    candidate_summary.write_text(original_summary)

    changed = json.loads(manifest.read_text())
    changed["stages"][1]["parent"] = None
    manifest.write_text(json.dumps(changed))
    immutable = subprocess.run(sync, capture_output=True, text=True)
    assert immutable.returncode != 0
    assert "parent is immutable" in immutable.stderr

    changed["stages"][1]["parent"] = "B0"
    manifest.write_text(json.dumps(changed))
    with sqlite3.connect(db) as connection:
        connection.execute(
            "UPDATE benchmark_runs SET run_dir = 'relocated' WHERE id = 'baseline-run'"
        )
    immutable_path = subprocess.run(sync, capture_output=True, text=True)
    assert immutable_path.returncode != 0
    assert "directory identifier is immutable" in immutable_path.stderr
