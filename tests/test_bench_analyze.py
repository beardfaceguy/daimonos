"""Unit tests for benchmarks/analyze-results.py aggregation (#928).

Pure-python: synthesizes result directories and exercises the analyzer's
run-discovery and cross-run statistics. No daimonos server needed.
"""

import importlib.util
import json
import os
import sys

import pytest


@pytest.fixture(scope="module")
def analyzer():
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    path = os.path.join(repo_root, "benchmarks", "analyze-results.py")
    spec = importlib.util.spec_from_file_location("analyze_results", path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["analyze_results"] = mod
    spec.loader.exec_module(mod)
    return mod


def _write_run(results_dir, name, tasks):
    """Create a fake run dir with one summary json per task."""
    run = results_dir / name
    run.mkdir(parents=True)
    for task_id, fields in tasks.items():
        summary = {
            "task_id": task_id,
            "task_name": task_id,
            "mode": "x",
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "tool_calls": 0,
            "mcp_tool_calls": 0,
            "cost_usd": 0.0,
            "wall_ms": 0,
            "success": True,
            "contaminated": False,
        }
        summary.update(fields)
        (run / f"{task_id}.json").write_text(json.dumps(summary))
    return run


# --- run discovery -------------------------------------------------------


def test_find_arm_runs_collects_rn_suffixed_dirs(analyzer, tmp_path):
    _write_run(tmp_path, "20260706-100000-daimonos-r1", {"t1": {}})
    _write_run(tmp_path, "20260706-100100-daimonos-r2", {"t1": {}})
    _write_run(tmp_path, "20260706-100200-daimonos-r3", {"t1": {}})
    runs = analyzer.find_arm_runs(tmp_path, "daimonos")
    assert len(runs) == 3, f"expected 3 runs, got {[r.name for r in runs]}"


def test_find_arm_runs_matches_untagged_single_run(analyzer, tmp_path):
    _write_run(tmp_path, "20260706-100000-baseline", {"t1": {}})
    runs = analyzer.find_arm_runs(tmp_path, "baseline")
    assert len(runs) == 1


def test_find_arm_runs_does_not_mix_arms(analyzer, tmp_path):
    _write_run(tmp_path, "20260706-100000-baseline-r1", {"t1": {}})
    _write_run(tmp_path, "20260706-100100-baseline-terse-r1", {"t1": {}})
    _write_run(tmp_path, "20260706-100200-daimonos-r1", {"t1": {}})
    base = analyzer.find_arm_runs(tmp_path, "baseline")
    terse = analyzer.find_arm_runs(tmp_path, "baseline-terse")
    assert len(base) == 1, f"baseline matched: {[r.name for r in base]}"
    assert len(terse) == 1, f"baseline-terse matched: {[r.name for r in terse]}"


def test_find_arm_runs_respects_tag(analyzer, tmp_path):
    _write_run(tmp_path, "20260706-100000-daimonos-mytag-r1", {"t1": {}})
    _write_run(tmp_path, "20260706-100100-daimonos-other-r1", {"t1": {}})
    runs = analyzer.find_arm_runs(tmp_path, "daimonos", tag="mytag")
    assert len(runs) == 1
    assert "mytag" in runs[0].name


def test_find_arm_runs_baseline_alias_cursor(analyzer, tmp_path):
    _write_run(tmp_path, "20260706-100000-cursor", {"t1": {}})
    runs = analyzer.find_arm_runs(tmp_path, "baseline")
    assert len(runs) == 1


# --- aggregation ---------------------------------------------------------


def test_aggregate_means_across_runs(analyzer, tmp_path):
    r1 = _write_run(tmp_path, "20260706-100000-daimonos-r1", {"t1": {"output_tokens": 100, "cost_usd": 0.10}})
    r2 = _write_run(tmp_path, "20260706-100100-daimonos-r2", {"t1": {"output_tokens": 300, "cost_usd": 0.30}})
    stats = analyzer.aggregate([r1, r2])
    t1 = stats["t1"]
    assert t1["n"] == 2
    assert t1["output_tokens"]["mean"] == 200
    assert t1["output_tokens"]["min"] == 100
    assert t1["output_tokens"]["max"] == 300
    assert abs(t1["cost_usd"]["mean"] - 0.20) < 1e-9


def test_aggregate_handles_task_missing_from_some_runs(analyzer, tmp_path):
    r1 = _write_run(tmp_path, "20260706-100000-daimonos-r1", {"t1": {"output_tokens": 100}, "t2": {"output_tokens": 50}})
    r2 = _write_run(tmp_path, "20260706-100100-daimonos-r2", {"t1": {"output_tokens": 200}})
    stats = analyzer.aggregate([r1, r2])
    assert stats["t1"]["n"] == 2
    assert stats["t2"]["n"] == 1
    assert stats["t2"]["output_tokens"]["mean"] == 50


def test_aggregate_tracks_success_rate_and_contamination(analyzer, tmp_path):
    r1 = _write_run(tmp_path, "20260706-100000-baseline-r1", {"t1": {"success": True}})
    r2 = _write_run(tmp_path, "20260706-100100-baseline-r2", {"t1": {"success": False, "contaminated": True}})
    stats = analyzer.aggregate([r1, r2])
    t1 = stats["t1"]
    assert t1["success_rate"] == 0.5
    assert t1["contaminated_runs"] == 1
