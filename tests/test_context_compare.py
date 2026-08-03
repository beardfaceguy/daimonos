import json
import subprocess
import sys
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "benchmarks" / "context_compare.py"


def write_summary(directory, name, *, calls, prompt_tokens, components):
    directory.mkdir(parents=True, exist_ok=True)
    (directory / f"{name}.json").write_text(json.dumps({
        "task_id": name,
        "model_slug": "model",
        "correct": True,
        "is_error": False,
        "llm_calls": calls,
        "prompt_tokens": prompt_tokens,
        "cache_read": prompt_tokens // 2,
        "calls_with_context": calls,
        "context_component_tokens_est_total": components,
    }))


def test_context_compare_separates_call_count_and_mean_context_effects(tmp_path):
    baseline = tmp_path / "baseline"
    candidate = tmp_path / "candidate"
    write_summary(
        baseline,
        "task",
        calls=10,
        prompt_tokens=1000,
        components={"system_bytes": 200, "tool_schema_bytes": 400},
    )
    write_summary(
        candidate,
        "task",
        calls=12,
        prompt_tokens=1080,
        components={"system_bytes": 240, "tool_schema_bytes": 300},
    )

    proc = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--baseline",
            str(baseline),
            "--candidate",
            str(candidate),
            "--json",
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert proc.returncode == 0, proc.stderr
    result = json.loads(proc.stdout)
    assert result["baseline"]["calls"] == 10
    assert result["candidate"]["calls"] == 12
    assert result["baseline"]["mean_prompt_tokens_per_call"] == 100
    assert result["candidate"]["mean_prompt_tokens_per_call"] == 90
    assert result["decomposition"]["total_prompt_token_delta"] == 80
    assert result["decomposition"]["call_count_effect"] == 190
    assert result["decomposition"]["mean_context_effect"] == -110
    assert result["decomposition"]["reconstructed_delta"] == 80
    assert result["candidate"]["ranked_components"][0] == [
        "tool_schema_bytes",
        300,
    ]


def test_context_compare_rejects_unmatched_task_runs(tmp_path):
    baseline = tmp_path / "baseline"
    candidate = tmp_path / "candidate"
    write_summary(
        baseline,
        "task-a",
        calls=2,
        prompt_tokens=100,
        components={},
    )
    write_summary(
        candidate,
        "task-b",
        calls=2,
        prompt_tokens=100,
        components={},
    )

    proc = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--baseline",
            str(baseline),
            "--candidate",
            str(candidate),
            "--json",
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert proc.returncode != 0
    assert "matching correctness-gated task runs" in proc.stderr
