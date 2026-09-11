"""Tests for the Daimonos SWE-bench runner."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

import pytest


RUNNER = (
    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "run_agent.py"
)


def load_runner():
    spec = importlib.util.spec_from_file_location("swebench_run_agent", RUNNER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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
