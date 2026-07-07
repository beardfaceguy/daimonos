"""CI smoke for the benchmark harness (#930): the zero-API-spend slice.

Exercises the full post-processing pipeline end to end — a synthetic results
tree through check-task.js and analyze-results.py as real subprocesses — plus
shell-syntax gates on the runner scripts, so the harness can't rot unnoticed
between (paid) live runs. The live-API slice is deliberately not in CI; the
run-all-arms.sh smoke gate covers it at launch time instead.
"""

import json
import os
import subprocess

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(REPO, "benchmarks")


def _mk_run(results_dir, name, task_id, *, out_tok, cost, mcp_calls, result_text):
    run = results_dir / name
    run.mkdir(parents=True)
    (run / f"{task_id}.raw.jsonl").write_text(
        json.dumps({"type": "result", "result": result_text}) + "\n"
    )
    (run / f"{task_id}.json").write_text(json.dumps({
        "task_id": task_id, "task_name": task_id, "mode": name.split("-")[2],
        "output_tokens": out_tok, "cache_read_tokens": 1000, "cache_write_tokens": 0,
        "tool_calls": 2, "mcp_tool_calls": mcp_calls, "cost_usd": cost,
        "wall_ms": 5000, "num_turns": 3, "is_error": False,
        "success": mcp_calls == 0 or "daimonos" in name,
        "contaminated": mcp_calls > 0 and "daimonos" not in name,
    }))
    return run


def test_runner_scripts_parse(daimonos=None):
    for script in ("run-benchmark.sh", "run-all-arms.sh"):
        proc = subprocess.run(["sh", "-n", os.path.join(BENCH, script)],
                              capture_output=True, text=True)
        assert proc.returncode == 0, f"{script} syntax: {proc.stderr}"


def test_checker_and_analyzer_end_to_end(tmp_path):
    results = tmp_path / "results"
    task_id = "01-read-understand"

    base_run = _mk_run(results, "20260706-100000-baseline-smoketest-r1", task_id,
                       out_tok=400, cost=0.02, mcp_calls=0,
                       result_text="Item has sku, name, category, quantity, unit_price; "
                                   "methods from_csv, find_by_sku, find_by_category, low_stock_items.")
    daim_run = _mk_run(results, "20260706-100100-daimonos-smoketest-r1", task_id,
                       out_tok=300, cost=0.015, mcp_calls=2,
                       result_text="sku, name, category, quantity, unit_price; "
                                   "from_csv find_by_sku find_by_category items_by_category")

    # Real checker binary against the real task definition
    for run in (base_run, daim_run):
        proc = subprocess.run(
            ["node", os.path.join(BENCH, "check-task.js"),
             os.path.join(BENCH, "tasks", f"{task_id}.json"),
             str(run / f"{task_id}.raw.jsonl"), str(tmp_path),
             str(run / f"{task_id}.json")],
            capture_output=True, text=True, timeout=60)
        assert proc.returncode == 0, f"checker: {proc.stderr}"
        stamped = json.loads((run / f"{task_id}.json").read_text())
        assert stamped["correct"] is True, f"calibration drift: {stamped}"

    # Real analyzer over the tree produces the aggregate comparison
    proc = subprocess.run(
        ["python3", os.path.join(BENCH, "analyze-results.py"), str(results), "smoketest"],
        capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0, f"analyzer: {proc.stderr}"
    assert "AGGREGATE" in proc.stdout
    assert "daimonos vs baseline" in proc.stdout


def test_analyzer_surfaces_contamination(tmp_path):
    results = tmp_path / "results"
    _mk_run(results, "20260706-100000-baseline-dirty-r1", "01-read-understand",
            out_tok=400, cost=0.02, mcp_calls=3, result_text="whatever")
    proc = subprocess.run(
        ["python3", os.path.join(BENCH, "analyze-results.py"), str(results), "dirty"],
        capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0
    assert "WARNING" in proc.stdout and "contaminated" in proc.stdout
