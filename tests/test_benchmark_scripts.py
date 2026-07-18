import csv
import json
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
CURSOR_ATTRIBUTE = REPO_ROOT / "benchmarks" / "cursor-attribute.py"


def run_attribution(tmp_path, summary, rows):
    run_dir = tmp_path / "run"
    run_dir.mkdir()
    summary_path = run_dir / "task.json"
    summary_path.write_text(json.dumps(summary))

    csv_path = tmp_path / "usage.csv"
    with csv_path.open("w", newline="") as csv_file:
        writer = csv.DictWriter(
            csv_file, fieldnames=["Date", "Model", "Total Tokens", "Cost"]
        )
        writer.writeheader()
        writer.writerows(rows)

    result = subprocess.run(
        ["python3", str(CURSOR_ATTRIBUTE), str(run_dir), str(csv_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(summary_path.read_text()), result.stdout


def summary(model_slug="claude-sonnet-5-high"):
    value = {
        "task_id": "04-explore-architecture",
        "runtime": "cursor",
        "started_at": "2026-07-14T03:16:20Z",
        "ended_at": "2026-07-14T03:17:30Z",
        "total_tokens": 696_744,
        "cost_usd": None,
    }
    if model_slug is not None:
        value["model_slug"] = model_slug
    return value


def test_cursor_attribution_ignores_other_models_in_task_window(tmp_path):
    attributed, output = run_attribution(
        tmp_path,
        summary(),
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
        ],
    )

    assert attributed["cursor_csv_rows"] == 1
    assert attributed["cursor_csv_total_tokens"] == 696_744
    assert attributed["cost_usd"] == 0.38
    assert attributed["cursor_csv_ignored_model_rows"] == 1
    assert "ignored_model_rows=1" in output


def test_cursor_attribution_wrong_model_only_leaves_cost_unset(tmp_path):
    stale_summary = summary()
    stale_summary.update(
        {
            "cost_usd": 9.99,
            "cursor_csv_total_tokens": 999,
            "cursor_csv_drift": 0.9,
        }
    )
    attributed, output = run_attribution(
        tmp_path,
        stale_summary,
        [
            {
                "Date": "2026-07-14T03:16:45.235Z",
                "Model": "gpt-5.6-sol-medium",
                "Total Tokens": "121687",
                "Cost": "0.09",
            }
        ],
    )

    assert attributed["cursor_csv_rows"] == 0
    assert attributed["cursor_csv_ignored_model_rows"] == 1
    assert attributed["cost_usd"] is None
    assert "cursor_csv_total_tokens" not in attributed
    assert "cursor_csv_drift" not in attributed
    assert "rows=0 ignored_model_rows=1" in output


def test_cursor_attribution_missing_model_slug_fails_closed(tmp_path):
    attributed, output = run_attribution(
        tmp_path,
        summary(model_slug=None),
        [
            {
                "Date": "2026-07-14T03:16:27.354Z",
                "Model": "claude-sonnet-5-high",
                "Total Tokens": "696744",
                "Cost": "0.38",
            }
        ],
    )

    assert attributed["cursor_csv_rows"] == 0
    assert attributed["cursor_csv_ignored_model_rows"] == 1
    assert attributed["cost_usd"] is None
    assert "missing model_slug" in output
