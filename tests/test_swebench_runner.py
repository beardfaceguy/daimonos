"""Tests for the Daimonos SWE-bench runner."""

from __future__ import annotations

import importlib.util
import json
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest


RUNNER = (
    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "run_agent.py"
)
BENCHMARK_CONFIG = (
    Path(__file__).resolve().parents[1]
    / "benchmarks"
    / "swebench"
    / "benchmark.toml"
)


def load_runner():
    spec = importlib.util.spec_from_file_location("swebench_run_agent", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_guard_limits_load_from_tracked_benchmark_config():
    runner = load_runner()

    config = runner.load_benchmark_config(BENCHMARK_CONFIG)

    assert config.keys() == {"runner", "guard", "experiment"}
    assert config["experiment"]["max_total_cost_usd"] > 0
    assert config["experiment"]["planning_spend_to_date_usd"] >= 0
    assert config["guard"]["max_instance_cost_usd"] > 0
    assert config["guard"]["max_instance_wall_seconds"] > 0
    assert config["runner"]["instance_timeout_seconds"] > 0
    assert config["runner"]["termination_grace_seconds"] > 0


def test_experiment_budget_reports_remaining_capacity():
    runner = load_runner()

    report = runner.experiment_budget_report(
        10.0,
        3.0,
        1.0,
    )

    assert report["max_total_cost_usd"] == 10.0
    assert report["planning_total_cost_usd"] == 4.0
    assert report["remaining_usd"] == 6.0
    assert report["cap_reached"] is False


def test_benchmark_config_rejects_unknown_keys(tmp_path):
    runner = load_runner()
    config = tmp_path / "benchmark.toml"
    config.write_text(BENCHMARK_CONFIG.read_text() + "\n[typo]\nlimit = 1\n")

    with pytest.raises(ValueError, match="unknown benchmark config section"):
        runner.load_benchmark_config(config)


def test_recover_run_cost_includes_partial_logs_without_double_counting(tmp_path):
    runner = load_runner()
    (tmp_path / "a.json").write_text(
        json.dumps({"task_id": "a", "cost_usd": 1.0})
    )
    for task_id, cost in (("a", "9.0"), ("b", "0.4")):
        log = tmp_path / f"{task_id}.bench" / "confighome" / "token-debug.log"
        log.parent.mkdir(parents=True)
        log.write_text(json.dumps({"cost_usd": cost}) + "\n")

    cost = runner.recover_run_cost(tmp_path)

    assert cost == pytest.approx(1.4)


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


def test_in_flight_budget_retries_after_provider_settlement_delay():
    runner = load_runner()
    responses = iter(
        [
            (
                subprocess.CompletedProcess([], 1),
                'Error: agent error: openrouter 402 Payment Required: '
                '{"error":{"message":"settling","code":402,"metadata":{"reason":'
                '"in_flight_budget_exhausted","headers":{"Retry-After":"120"}}}}',
            ),
            (subprocess.CompletedProcess([], 0), ""),
        ]
    )
    attempts = []
    sleeps = []

    def run_once(attempt):
        attempts.append(attempt)
        return next(responses)

    completed, retry_count = runner.run_with_in_flight_retry(
        run_once,
        max_retries=1,
        max_retry_after=300,
        sleep=sleeps.append,
    )

    assert completed.returncode == 0
    assert retry_count == 1
    assert attempts == [0, 1]
    assert sleeps == [120]


@pytest.mark.parametrize(
    "error_text",
    [
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"message":"insufficient credits","code":402,"metadata":'
        '{"headers":{"Retry-After":"120"}}}}',
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"code":402,"metadata":{"reason":'
        '"in_flight_budget_exhausted"}}}',
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"code":402,"metadata":{"reason":'
        '"in_flight_budget_exhausted","headers":{"Retry-After":"999"}}}}',
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"metadata":{"reason":"in_flight_budget_exhausted"}}} '
        '{"headers":{"Retry-After":"120"}}',
    ],
)
def test_other_billing_errors_and_unsafe_delays_are_not_retried(error_text):
    runner = load_runner()
    attempts = []
    sleeps = []

    def run_once(attempt):
        attempts.append(attempt)
        return subprocess.CompletedProcess([], 1), error_text

    completed, retry_count = runner.run_with_in_flight_retry(
        run_once,
        max_retries=1,
        max_retry_after=300,
        sleep=sleeps.append,
    )

    assert completed.returncode == 1
    assert retry_count == 0
    assert attempts == [0]
    assert sleeps == []


def test_in_flight_retry_exhaustion_is_bounded():
    runner = load_runner()
    error = (
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"code":402,"metadata":{"reason":'
        '"in_flight_budget_exhausted","headers":{"Retry-After":"1"}}}}'
    )
    attempts = []
    sleeps = []

    def run_once(attempt):
        attempts.append(attempt)
        return subprocess.CompletedProcess([], 1), error

    completed, retry_count = runner.run_with_in_flight_retry(
        run_once,
        max_retries=2,
        max_retry_after=300,
        sleep=sleeps.append,
    )

    assert completed.returncode == 1
    assert retry_count == 2
    assert attempts == [0, 1, 2]
    assert sleeps == [1, 1]


def test_in_flight_retry_accepts_immediate_provider_hint():
    runner = load_runner()
    error = (
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"code":402,"metadata":{"reason":'
        '"in_flight_budget_exhausted","headers":{"Retry-After":"0"}}}}'
    )

    assert runner.in_flight_retry_after(error, max_retry_after=300) == 0


@pytest.mark.parametrize("returncode", [124, 137])
def test_in_flight_retry_does_not_retry_killed_attempts(returncode):
    runner = load_runner()
    error = (
        'Error: agent error: openrouter 402 Payment Required: '
        '{"error":{"code":402,"metadata":{"reason":'
        '"in_flight_budget_exhausted","headers":{"Retry-After":"120"}}}}'
    )
    attempts = []
    sleeps = []

    def run_once(attempt):
        attempts.append(attempt)
        return subprocess.CompletedProcess([], returncode), error

    completed, retry_count = runner.run_with_in_flight_retry(
        run_once,
        max_retries=2,
        max_retry_after=300,
        sleep=sleeps.append,
    )

    assert completed.returncode == returncode
    assert retry_count == 0
    assert attempts == [0]
    assert sleeps == []


def test_snapshot_tool_trace_creates_private_standalone_database(tmp_path):
    runner = load_runner()
    source = tmp_path / "state" / "analytics.db"
    source.parent.mkdir()
    with sqlite3.connect(source) as connection:
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute(
            "CREATE TABLE tool_calls (id INTEGER PRIMARY KEY, tool_name TEXT)"
        )
        connection.execute(
            "INSERT INTO tool_calls (tool_name) VALUES (?)",
            ("read_file",),
        )
        connection.commit()
        destination = tmp_path / "trace.sqlite"

        row_count = runner.snapshot_tool_trace(source, destination)

    assert row_count == 1
    assert destination.stat().st_mode & 0o777 == 0o600
    with sqlite3.connect(destination) as connection:
        assert connection.execute(
            "SELECT tool_name FROM tool_calls"
        ).fetchall() == [("read_file",)]


def test_snapshot_tool_trace_missing_source_leaves_no_artifact(tmp_path):
    runner = load_runner()
    destination = tmp_path / "trace.sqlite"

    row_count = runner.snapshot_tool_trace(
        tmp_path / "missing-analytics.db",
        destination,
    )

    assert row_count is None
    assert not destination.exists()


def test_snapshot_tool_trace_keeps_unknown_schema_snapshot(tmp_path):
    runner = load_runner()
    source = tmp_path / "analytics.db"
    with sqlite3.connect(source) as connection:
        connection.execute("CREATE TABLE future_schema (value TEXT)")
    destination = tmp_path / "trace.sqlite"

    row_count = runner.snapshot_tool_trace(source, destination)

    assert row_count is None
    assert destination.exists()
    assert destination.stat().st_mode & 0o777 == 0o600


def test_tool_trace_summary_exposes_missing_and_unknown_snapshots():
    runner = load_runner()

    summary = runner.summarize_tool_traces(
        [
            {"tool_trace_file": "a.sqlite", "tool_trace_rows": 7},
            {"tool_trace_file": "b.sqlite", "tool_trace_rows": None},
            {"tool_trace_file": None, "tool_trace_rows": None},
        ],
        expected=3,
    )

    assert summary == {
        "expected": 3,
        "captured": 2,
        "missing": 1,
        "unknown_schema": 1,
        "total_rows": 7,
    }


def test_guard_report_marks_cost_without_stopping_the_run():
    runner = load_runner()

    report = runner.guard_report(
        {"cost_usd": 2.5, "wall_ms": 250_000, "correct": True},
        cost_limit=2.0,
        wall_limit_seconds=300,
    )

    assert report == {
        "mode": "report",
        "cost_limit_usd": 2.0,
        "wall_limit_ms": 300_000,
        "cost_observed_usd": 2.5,
        "wall_observed_ms": 250_000,
        "cost_would_trigger": True,
        "wall_would_trigger": False,
        "would_trigger": True,
    }


def test_guard_report_accepts_float_wall_at_exact_boundaries():
    runner = load_runner()

    report = runner.guard_report(
        {"cost_usd": 2.0, "wall_ms": 300_000.0},
        cost_limit=2.0,
        wall_limit_seconds=300,
    )

    assert report["cost_would_trigger"] is True
    assert report["wall_would_trigger"] is True
    assert report["would_trigger"] is True


def test_guard_report_preserves_unknown_observations():
    runner = load_runner()

    report = runner.guard_report(
        {"cost_usd": None, "wall_ms": None},
        cost_limit=2.0,
        wall_limit_seconds=300,
    )

    assert report["cost_would_trigger"] is None
    assert report["wall_would_trigger"] is None
    assert report["would_trigger"] is False


def test_guard_summary_exposes_missing_report_coverage():
    runner = load_runner()

    summary = runner.summarize_guard_reports(
        [
            {
                "task_id": "a",
                "guard": {
                    "would_trigger": True,
                    "cost_would_trigger": True,
                    "wall_would_trigger": False,
                },
            },
            {"task_id": "b", "guard": None},
        ]
    )

    assert summary["instances"] == 2
    assert summary["reported"] == 1
    assert summary["missing_guard_data"] == 1
    assert summary["would_trigger"] == ["a"]
