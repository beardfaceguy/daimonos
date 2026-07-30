"""Tests for benchmarks/check_task.py — the per-task correctness gate (#929).

Invokes the checker with synthetic task/raw/summary files and asserts the
summary is stamped with checks_passed / checks_total / correct. (Ported from
the check-task.js version when the harness moved off JavaScript, vikunja #1126.)
"""

import json
import os
import subprocess
import sys


CHECKER = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "benchmarks", "check_task.py",
)


def run_checker(tmp_path, checks, final_text, workspace_files=None, raw_format=None):
    """Write synthetic inputs, run check_task.py, return the updated summary."""
    task_file = tmp_path / "task.json"
    task_file.write_text(json.dumps({"id": "t", "name": "t", "checks": checks}))

    raw_file = tmp_path / "t.raw.jsonl"
    raw_file.write_text(json.dumps({"type": "result", "result": final_text}) + "\n")

    workspace = tmp_path / "ws"
    workspace.mkdir(exist_ok=True)
    for rel, content in (workspace_files or {}).items():
        (workspace / rel).write_text(content)

    summary_file = tmp_path / "t.json"
    summary_file.write_text(json.dumps({"task_id": "t", "success": True}))

    cmd = [sys.executable, CHECKER, str(task_file), str(raw_file), str(workspace), str(summary_file)]
    if raw_format is not None:
        cmd.append(raw_format)
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    assert proc.returncode == 0, f"checker failed: {proc.stderr}"
    return json.loads(summary_file.read_text())


def test_response_all_patterns_pass(tmp_path):
    s = run_checker(
        tmp_path,
        [{"type": "response", "all": ["sku", "unit_price"]}],
        "The Item struct has sku, name, and unit_price fields.",
    )
    assert s["correct"] is True
    assert s["checks_passed"] == 1
    assert s["checks_total"] == 1


def test_response_all_patterns_fail(tmp_path):
    s = run_checker(
        tmp_path,
        [{"type": "response", "all": ["sku", "unit_price"]}],
        "The Item struct has a sku field.",
    )
    assert s["correct"] is False
    assert s["checks_passed"] == 0


def test_response_patterns_case_insensitive(tmp_path):
    s = run_checker(
        tmp_path,
        [{"type": "response", "all": ["SKU"]}],
        "the item has a sku field",
    )
    assert s["correct"] is True


def test_response_any_min(tmp_path):
    checks = [{"type": "response", "any": ["from_csv", "find_by_sku", "low_stock_items"], "min": 2}]
    s = run_checker(tmp_path, checks, "It exposes from_csv and find_by_sku.")
    assert s["correct"] is True
    s = run_checker(tmp_path, checks, "It exposes from_csv only.")
    assert s["correct"] is False


def test_workspace_command_pass_and_fail(tmp_path):
    s = run_checker(
        tmp_path,
        [{"type": "workspace", "command": "grep -q display_names cfg.txt"}],
        "done",
        workspace_files={"cfg.txt": "display_names = true\n"},
    )
    assert s["correct"] is True

    s = run_checker(
        tmp_path,
        [{"type": "workspace", "command": "grep -q display_names cfg.txt"}],
        "done",
        workspace_files={"cfg.txt": "category_labels = true\n"},
    )
    assert s["correct"] is False


def test_mixed_checks_all_must_pass(tmp_path):
    s = run_checker(
        tmp_path,
        [
            {"type": "response", "all": ["renamed"]},
            {"type": "workspace", "command": "test -f cfg.txt"},
        ],
        "I renamed the field.",
        workspace_files={"cfg.txt": "x\n"},
    )
    assert s["correct"] is True
    assert s["checks_passed"] == 2
    assert s["checks_total"] == 2


def test_no_checks_leaves_correct_null(tmp_path):
    s = run_checker(tmp_path, [], "anything")
    assert s["checks_total"] == 0
    assert s["correct"] is None


def test_missing_result_event_fails_response_checks(tmp_path):
    """A truncated raw stream (no result event) cannot pass response checks."""
    task_file = tmp_path / "task.json"
    task_file.write_text(json.dumps({"id": "t", "checks": [{"type": "response", "all": ["x"]}]}))
    raw_file = tmp_path / "t.raw.jsonl"
    raw_file.write_text("")
    ws = tmp_path / "ws"
    ws.mkdir()
    summary_file = tmp_path / "t.json"
    summary_file.write_text(json.dumps({"task_id": "t"}))
    proc = subprocess.run(
        [sys.executable, CHECKER, str(task_file), str(raw_file), str(ws), str(summary_file)],
        capture_output=True, text=True, timeout=120,
    )
    assert proc.returncode == 0
    s = json.loads(summary_file.read_text())
    assert s["correct"] is False


def test_text_format_uses_whole_stdout_as_response(tmp_path):
    """daimonos writes plain assistant text (not events); the `text` format
    treats the whole raw file as the response."""
    task_file = tmp_path / "task.json"
    task_file.write_text(json.dumps({"id": "t", "checks": [{"type": "response", "all": ["hello"]}]}))
    raw_file = tmp_path / "t.raw.txt"
    raw_file.write_text("plain hello world, no json here")
    ws = tmp_path / "ws"
    ws.mkdir()
    summary_file = tmp_path / "t.json"
    summary_file.write_text(json.dumps({"task_id": "t"}))
    proc = subprocess.run(
        [sys.executable, CHECKER, str(task_file), str(raw_file), str(ws), str(summary_file), "text"],
        capture_output=True, text=True, timeout=120,
    )
    assert proc.returncode == 0
    assert json.loads(summary_file.read_text())["correct"] is True
