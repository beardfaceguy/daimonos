"""Tests for mini-swe-agent SWE-bench summary normalization."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest


SCRIPT = (
    Path(__file__).resolve().parents[1] / "benchmarks" / "swebench" / "extract_mini.py"
)


def _extract(tmp_path, payload):
    trajectory = tmp_path / "trajectory.json"
    output = tmp_path / "summary.json"
    trajectory.write_text(json.dumps(payload))
    subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            str(trajectory),
            "django__django-11815",
            "django/django",
            "openrouter/anthropic/claude-opus-4.8",
            str(output),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    return json.loads(output.read_text())


def test_extract_mini_sums_openrouter_reported_cost(tmp_path):
    summary = _extract(
        tmp_path,
        {
            "messages": [
                {
                    "extra": {
                        "timestamp": 10.0,
                        "response": {
                            "usage": {
                                "prompt_tokens": 100,
                                "completion_tokens": 10,
                                "cost": 0.0125,
                            }
                        },
                    }
                },
                {
                    "extra": {
                        "timestamp": 12.5,
                        "response": {
                            "usage": {
                                "prompt_tokens": 120,
                                "completion_tokens": 20,
                                "cost": 0.0175,
                            }
                        },
                    }
                },
            ],
            "info": {
                "exit_status": "Submitted",
                "model_stats": {"instance_cost": 0.03},
            },
        },
    )

    assert summary["total_tokens"] == 250
    assert summary["llm_calls"] == 2
    assert summary["cost_usd"] == pytest.approx(0.03)
    assert summary["cost_source"] == "openrouter_usage"
    assert summary["model_stats_cost_usd"] == pytest.approx(0.03)
    assert summary["cost_matches_model_stats"] is True
    assert summary["wall_ms"] == 2500


def test_extract_mini_rejects_partial_provider_cost(tmp_path):
    summary = _extract(
        tmp_path,
        {
            "messages": [
                {
                    "extra": {
                        "response": {
                            "usage": {
                                "prompt_tokens": 100,
                                "completion_tokens": 10,
                                "cost": 0.0125,
                            }
                        }
                    }
                },
                {
                    "extra": {
                        "response": {
                            "usage": {
                                "prompt_tokens": 120,
                                "completion_tokens": 20,
                            }
                        }
                    }
                },
            ],
            "info": {"exit_status": "Submitted"},
        },
    )

    assert summary["cost_usd"] is None
    assert summary["cost_source"] is None


@pytest.mark.parametrize("bad_cost", [True, -0.01])
def test_extract_mini_rejects_invalid_provider_cost(tmp_path, bad_cost):
    summary = _extract(
        tmp_path,
        {
            "messages": [
                {
                    "extra": {
                        "response": {
                            "usage": {
                                "prompt_tokens": 100,
                                "completion_tokens": 10,
                                "cost": bad_cost,
                            }
                        }
                    }
                }
            ],
            "info": {"exit_status": "Submitted"},
        },
    )

    assert summary["llm_calls"] == 1
    assert summary["cost_usd"] is None


def test_extract_mini_rejects_generation_without_usage(tmp_path):
    summary = _extract(
        tmp_path,
        {
            "messages": [
                {
                    "extra": {
                        "response": {
                            "usage": {
                                "prompt_tokens": 100,
                                "completion_tokens": 10,
                                "cost": 0.0125,
                            }
                        }
                    }
                },
                {"extra": {"response": {}}},
            ],
            "info": {"exit_status": "Submitted"},
        },
    )

    assert summary["llm_calls"] == 2
    assert summary["cost_usd"] is None


def test_extract_mini_reports_no_cost_without_generations(tmp_path):
    summary = _extract(
        tmp_path,
        {"messages": [{"role": "user", "content": "task"}], "info": {}},
    )

    assert summary["llm_calls"] == 0
    assert summary["cost_usd"] is None
    assert summary["cost_source"] is None


def test_extract_mini_flags_model_stats_cost_mismatch(tmp_path):
    summary = _extract(
        tmp_path,
        {
            "messages": [
                {
                    "extra": {
                        "response": {
                            "usage": {
                                "prompt_tokens": 100,
                                "completion_tokens": 10,
                                "cost": 0.0125,
                            }
                        }
                    }
                }
            ],
            "info": {"model_stats": {"instance_cost": 0.02}},
        },
    )

    assert summary["model_stats_cost_usd"] == pytest.approx(0.02)
    assert summary["cost_matches_model_stats"] is False
