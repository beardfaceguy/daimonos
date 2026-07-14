import csv
import json
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
CURSOR_ATTRIBUTE = REPO_ROOT / "benchmarks" / "cursor-attribute.py"


def test_cursor_attribution_ignores_other_models_in_task_window(tmp_path):
    run_dir = tmp_path / "run"
    run_dir.mkdir()
    summary_path = run_dir / "04-explore-architecture.json"
    summary_path.write_text(
        json.dumps(
            {
                "task_id": "04-explore-architecture",
                "runtime": "cursor",
                "model_slug": "claude-sonnet-5-high",
                "started_at": "2026-07-14T03:16:20Z",
                "ended_at": "2026-07-14T03:17:30Z",
                "total_tokens": 696_744,
                "cost_usd": None,
            }
        )
    )

    csv_path = tmp_path / "usage.csv"
    fieldnames = [
        "Date",
        "Model",
        "Total Tokens",
        "Cost",
    ]
    with csv_path.open("w", newline="") as csv_file:
        writer = csv.DictWriter(csv_file, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(
            [
                {
                    "Date": "2026-07-14T03:16:27.354Z",
                    "Model": "claude-sonnet-5-high",
                    "Total Tokens": "696744",
                    "Cost": "0.38",
                },
                {
                    "Date": "2026-07-14T03:16:45.235Z",
                    "Model": "gpt-5.6-sol-medium",
                    "Total Tokens": "121687",
                    "Cost": "0.09",
                },
            ]
        )

    result = subprocess.run(
        [
            "python3",
            str(CURSOR_ATTRIBUTE),
            str(run_dir),
            str(csv_path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )

    attributed = json.loads(summary_path.read_text())
    assert attributed["cursor_csv_rows"] == 1
    assert attributed["cursor_csv_total_tokens"] == 696_744
    assert attributed["cost_usd"] == 0.38
    assert attributed["cursor_csv_ignored_model_rows"] == 1
    assert "ignored_model_rows=1" in result.stdout
