"""Tests for benchmarks/extract_tokens.py — the per-task token normalizer.

The former extract-tokens.js had no test coverage; the Python port (vikunja
#1126) adds it. Exercises each runtime branch (claude / cursor / daimonos) plus
the shared total/error accounting, by running the script on synthetic raw
streams and asserting the emitted summary JSON.
"""

import json
import os
import subprocess
import sys


EXTRACTOR = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "benchmarks", "extract_tokens.py",
)


def run_extractor(tmp_path, runtime, raw_text, tokenlog_text=None,
                  started="2026-01-01T00:00:00Z", ended="2026-01-01T00:05:00Z",
                  wall_ms="60000", exit_code="0"):
    raw_file = tmp_path / "t.raw"
    raw_file.write_text(raw_text)
    if tokenlog_text is None:
        tokenlog = "-"
    else:
        tl = tmp_path / "t.tokenlog.jsonl"
        tl.write_text(tokenlog_text)
        tokenlog = str(tl)
    out_file = tmp_path / "t.json"
    cmd = [
        sys.executable, EXTRACTOR, runtime, str(raw_file), tokenlog,
        "task-id", "Task Name", "model/slug", "canon/model",
        started, ended, wall_ms, exit_code, str(out_file),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0, (
        f"extractor failed (rc={proc.returncode})\nstdout: {proc.stdout}\nstderr: {proc.stderr}"
    )
    return json.loads(out_file.read_text())


def test_claude_usage_and_total(tmp_path):
    raw = json.dumps({
        "type": "result", "is_error": False, "total_cost_usd": 0.0123,
        "usage": {
            "input_tokens": 100, "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 300, "output_tokens": 50,
        },
    }) + "\n"
    s = run_extractor(tmp_path, "claude", raw)
    assert s["input"] == 100
    assert s["cache_write"] == 20
    assert s["cache_read"] == 300
    assert s["output"] == 50
    assert s["total_tokens"] == 470  # 100+20+300+50
    assert s["cost_usd"] == 0.0123
    assert s["is_error"] is False
    assert s["success"] is True
    assert s["runtime"] == "claude"


def test_cursor_camelcase_usage_cost_null(tmp_path):
    raw = json.dumps({
        "type": "result", "is_error": False,
        "usage": {
            "inputTokens": 5, "cacheWriteTokens": 10,
            "cacheReadTokens": 200, "outputTokens": 7,
        },
    }) + "\n"
    s = run_extractor(tmp_path, "cursor", raw)
    assert (s["input"], s["cache_write"], s["cache_read"], s["output"]) == (5, 10, 200, 7)
    assert s["total_tokens"] == 222
    assert s["cost_usd"] is None  # cursor emits no per-run cost
    assert s["is_error"] is False


def test_codex_turn_completed_usage_and_fresh_input(tmp_path):
    # Codex reports usage on turn.completed; input_tokens is the prompt TOTAL
    # (includes cached), so fresh input = total - cache_write - cache_read.
    raw = "\n".join([
        json.dumps({"type": "thread.started"}),
        json.dumps({"type": "item.completed",
                    "item": {"id": "i0", "type": "agent_message", "text": "done"}}),
        json.dumps({"type": "turn.completed", "usage": {
            "input_tokens": 1000, "cached_input_tokens": 600,
            "cache_write_input_tokens": 300, "output_tokens": 40,
        }}),
    ]) + "\n"
    s = run_extractor(tmp_path, "codex", raw)
    assert s["cache_write"] == 300
    assert s["cache_read"] == 600
    assert s["input"] == 100      # 1000 - 300 - 600 (fresh remainder)
    assert s["output"] == 40
    assert s["total_tokens"] == 1040  # 100+300+600+40 == prompt_total+output
    assert s["cost_usd"] is None
    assert s["tool_calls"] == 0
    assert s["is_error"] is False


def test_codex_no_turn_completed_is_error(tmp_path):
    # A truncated codex stream with no turn.completed event -> error.
    raw = json.dumps({"type": "thread.started"}) + "\n"
    s = run_extractor(tmp_path, "codex", raw)
    assert s["is_error"] is True
    assert s["total_tokens"] == 0


def test_codex_fresh_input_floored_at_zero(tmp_path):
    # If cached subsets exceed the reported prompt total (double-report),
    # fresh input floors at 0 rather than going negative.
    raw = "\n".join([
        json.dumps({"type": "turn.completed", "usage": {
            "input_tokens": 100, "cached_input_tokens": 80,
            "cache_write_input_tokens": 50, "output_tokens": 5,
        }}),
    ]) + "\n"
    s = run_extractor(tmp_path, "codex", raw)
    assert s["input"] == 0  # max(0, 100 - 50 - 80)


def test_daimonos_sums_tokenlog_within_window(tmp_path):
    # Two in-window call lines + one out-of-window line that must be excluded.
    lines = [
        json.dumps({"ts": "2026-01-01T00:01:00Z", "input": 10, "output": 5,
                    "cache_write": 1, "cache_read": 2, "cost_usd": "0.0010"}),
        json.dumps({"ts": "2026-01-01T00:02:00Z", "input": 20, "output": 6,
                    "cache_write": 0, "cache_read": 4, "cost_usd": "0.0020"}),
        # far outside the started/ended window -> excluded
        json.dumps({"ts": "2027-01-01T00:00:00Z", "input": 999, "output": 999,
                    "cost_usd": "9.9999"}),
        # a structured non-call event line -> skipped
        json.dumps({"event": "compaction", "evicted": 3}),
    ]
    s = run_extractor(tmp_path, "daimonos", "irrelevant stdout",
                      tokenlog_text="\n".join(lines) + "\n")
    assert s["input"] == 30      # 10+20, 999 excluded by window
    assert s["output"] == 11     # 5+6
    assert s["cache_write"] == 1
    assert s["cache_read"] == 6
    assert s["total_tokens"] == 48
    assert s["llm_calls"] == 2   # window-filtered call count
    assert s["tool_calls"] is None
    assert s["is_error"] is False
    assert abs(s["cost_usd"] - 0.0030) < 1e-9
    assert s["calls_with_context"] == 0
    assert s["context_coverage_pct"] == 0
    assert s["context_estimated_tokens_total"] is None
    assert s["context_component_bytes_total"] is None


def test_daimonos_aggregates_additive_context_composition(tmp_path):
    lines = [
        json.dumps({
            "ts": "2026-01-01T00:01:00Z",
            "input": 10,
            "output": 5,
            "cache_write": 1,
            "cache_read": 2,
            "stop_reason": "tool_use",
            "context": {
                "payload_tokens_est": 100,
                "system_bytes": 40,
                "tool_schema_bytes": 80,
                "tool_result_ok_bytes": 0,
            },
        }),
        json.dumps({
            "ts": "2026-01-01T00:02:00Z",
            "input": 20,
            "output": 6,
            "cache_write": 0,
            "cache_read": 4,
            "stop_reason": "end_turn",
            "context": {
                "payload_tokens_est": 150,
                "system_bytes": 40,
                "tool_schema_bytes": 80,
                "tool_result_ok_bytes": 120,
            },
        }),
        # Legacy call line: usage still counts, context coverage does not.
        json.dumps({
            "ts": "2026-01-01T00:02:30Z",
            "input": 5,
            "output": 1,
            "cache_write": 0,
            "cache_read": 0,
        }),
    ]

    s = run_extractor(
        tmp_path,
        "daimonos",
        "stdout",
        tokenlog_text="\n".join(lines) + "\n",
    )

    assert s["llm_calls"] == 3
    assert s["prompt_tokens"] == 42
    assert s["fresh_input_tokens"] == 35
    assert s["mean_prompt_tokens_per_call"] == 14
    assert s["calls_with_context"] == 2
    assert abs(s["context_coverage_pct"] - (200 / 3)) < 1e-9
    assert s["context_estimated_tokens_total"] == 250
    assert s["context_estimated_tokens_mean"] == 125
    assert s["context_estimated_tokens_first"] == 100
    assert s["context_estimated_tokens_last"] == 150
    assert s["context_estimated_tokens_max"] == 150
    assert s["context_growth_tokens_per_call"] == 50
    assert s["tool_loop_calls"] == 1
    assert s["final_calls"] == 1
    assert s["failed_calls"] == 0
    assert s["context_component_bytes_total"] == {
        "system_bytes": 80,
        "tool_result_ok_bytes": 120,
        "tool_schema_bytes": 160,
    }
    assert s["context_component_tokens_est_total"] == {
        "system_bytes": 20,
        "tool_result_ok_bytes": 30,
        "tool_schema_bytes": 40,
    }


def test_daimonos_no_calls_is_error(tmp_path):
    s = run_extractor(tmp_path, "daimonos", "stdout", tokenlog_text="")
    assert s["llm_calls"] == 0
    assert s["is_error"] is True  # no call lines seen -> error
    assert s["success"] is False


def test_nonzero_exit_forces_error(tmp_path):
    raw = json.dumps({"type": "result", "is_error": False,
                      "usage": {"input_tokens": 1, "output_tokens": 1}}) + "\n"
    s = run_extractor(tmp_path, "claude", raw, exit_code="1")
    assert s["is_error"] is True


def test_unknown_runtime_errors(tmp_path):
    out_file = tmp_path / "t.json"
    raw_file = tmp_path / "t.raw"
    raw_file.write_text("")
    proc = subprocess.run(
        [sys.executable, EXTRACTOR, "bogus", str(raw_file), "-", "i", "n",
         "m", "c", "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z", "1000", "0", str(out_file)],
        capture_output=True, text=True, timeout=60,
    )
    assert proc.returncode == 2
    assert "unknown runtime" in proc.stderr
