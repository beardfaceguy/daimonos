#!/usr/bin/env python3
"""Replay report-only per-instance guard thresholds over summary directories."""

import argparse
import json
import pathlib

import run_agent


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cost-limit", type=float, required=True)
    parser.add_argument("--wall-limit", type=int, required=True)
    parser.add_argument(
        "--evaluator-report",
        action="append",
        default=[],
        type=pathlib.Path,
    )
    parser.add_argument("run_dirs", nargs="+", type=pathlib.Path)
    args = parser.parse_args()
    if args.cost_limit <= 0:
        parser.error("--cost-limit must be positive")
    if args.wall_limit <= 0:
        parser.error("--wall-limit must be positive")

    rows = {}
    for run_dir in args.run_dirs:
        for path in sorted(run_dir.glob("*.json")):
            try:
                row = json.loads(path.read_text())
            except (OSError, ValueError):
                continue
            task_id = row.get("task_id")
            if not task_id:
                continue
            if path.name != f"{task_id}.json":
                continue
            if task_id in rows:
                parser.error(f"duplicate task summary: {task_id}")
            rows[task_id] = row

    correctness = {}
    for path in args.evaluator_report:
        report = json.loads(path.read_text())
        for task_id in report.get("resolved_ids", []):
            correctness[task_id] = True
        for task_id in report.get("unresolved_ids", []):
            correctness[task_id] = False
    for task_id, row in rows.items():
        if isinstance(row.get("correct"), bool):
            if (
                task_id in correctness
                and correctness[task_id] != row["correct"]
            ):
                parser.error(f"correctness conflict for {task_id}")
            correctness[task_id] = row["correct"]

    reports = {
        task_id: run_agent.guard_report(
            row,
            cost_limit=args.cost_limit,
            wall_limit_seconds=args.wall_limit,
        )
        for task_id, row in rows.items()
    }
    triggered = sorted(
        task_id for task_id, report in reports.items() if report["would_trigger"]
    )
    output = {
        "cost_limit_usd": args.cost_limit,
        "wall_limit_ms": args.wall_limit * 1000,
        "instances": len(rows),
        "observed_cost_usd": sum(
            row["cost_usd"]
            for row in rows.values()
            if isinstance(row.get("cost_usd"), (int, float))
            and not isinstance(row["cost_usd"], bool)
        ),
        "would_trigger": triggered,
        "cost_would_trigger": sorted(
            task_id
            for task_id, report in reports.items()
            if report["cost_would_trigger"] is True
        ),
        "wall_would_trigger": sorted(
            task_id
            for task_id, report in reports.items()
            if report["wall_would_trigger"] is True
        ),
        "correct_would_trigger": sorted(
            task_id for task_id in triggered if correctness.get(task_id) is True
        ),
        "incorrect_would_trigger": sorted(
            task_id for task_id in triggered if correctness.get(task_id) is False
        ),
        "unknown_correctness_would_trigger": sorted(
            task_id
            for task_id in triggered
            if not isinstance(correctness.get(task_id), bool)
        ),
    }
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
