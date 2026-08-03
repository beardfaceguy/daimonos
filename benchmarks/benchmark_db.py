#!/usr/bin/env python3
"""Initialize, synchronize, and compare Daimonos optimization benchmarks."""

import argparse
import json
import os
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path


SCHEMA = """
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS benchmark_stages (
    id TEXT PRIMARY KEY,
    parent_id TEXT REFERENCES benchmark_stages(id),
    feature TEXT NOT NULL,
    scope_fingerprint TEXT NOT NULL,
    benchmark_kind TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    thinking TEXT,
    task_ids_json TEXT NOT NULL,
    binary_sha256 TEXT,
    git_commit TEXT,
    report_path TEXT,
    notes TEXT,
    created_at TEXT
);

CREATE TABLE IF NOT EXISTS benchmark_runs (
    id TEXT PRIMARY KEY,
    run_dir TEXT NOT NULL UNIQUE,
    started_at TEXT,
    ended_at TEXT,
    task_count INTEGER NOT NULL,
    total_tokens REAL NOT NULL,
    prompt_tokens REAL NOT NULL,
    fresh_input_tokens REAL NOT NULL,
    cache_write_tokens REAL NOT NULL,
    cache_read_tokens REAL NOT NULL,
    output_tokens REAL NOT NULL,
    llm_calls REAL NOT NULL,
    cost_usd REAL,
    wall_ms REAL NOT NULL,
    correct_tasks INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS stage_runs (
    stage_id TEXT NOT NULL REFERENCES benchmark_stages(id),
    run_id TEXT NOT NULL REFERENCES benchmark_runs(id),
    PRIMARY KEY (stage_id, run_id)
);

CREATE TABLE IF NOT EXISTS task_results (
    run_id TEXT NOT NULL REFERENCES benchmark_runs(id),
    task_id TEXT NOT NULL,
    task_name TEXT,
    total_tokens REAL NOT NULL,
    prompt_tokens REAL NOT NULL,
    fresh_input_tokens REAL NOT NULL,
    cache_write_tokens REAL NOT NULL,
    cache_read_tokens REAL NOT NULL,
    output_tokens REAL NOT NULL,
    llm_calls REAL NOT NULL,
    cost_usd REAL,
    wall_ms REAL NOT NULL,
    correct INTEGER,
    checks_passed INTEGER,
    checks_total INTEGER,
    context_components_json TEXT,
    raw_json TEXT NOT NULL,
    PRIMARY KEY (run_id, task_id)
);

CREATE TABLE IF NOT EXISTS stage_metrics (
    stage_id TEXT NOT NULL REFERENCES benchmark_stages(id),
    metric TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'aggregate',
    value REAL NOT NULL,
    unit TEXT NOT NULL,
    PRIMARY KEY (stage_id, metric, scope)
);

CREATE TABLE IF NOT EXISTS benchmark_artifacts (
    stage_id TEXT NOT NULL REFERENCES benchmark_stages(id),
    path TEXT NOT NULL,
    sha256 TEXT,
    kind TEXT,
    PRIMARY KEY (stage_id, path)
);

CREATE INDEX IF NOT EXISTS idx_stage_scope
    ON benchmark_stages(scope_fingerprint);
CREATE INDEX IF NOT EXISTS idx_task_id
    ON task_results(task_id);
"""


STAGE_FIELDS = [
    "feature",
    "scope_fingerprint",
    "benchmark_kind",
    "provider",
    "model",
    "thinking",
    "task_ids_json",
    "binary_sha256",
    "git_commit",
    "report_path",
    "notes",
    "created_at",
]


def connect(path):
    path = Path(path).expanduser()
    old_umask = os.umask(0o077)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(path)
    finally:
        os.umask(old_umask)
    connection.executescript(SCHEMA)
    connection.execute("PRAGMA foreign_keys = ON")
    connection.commit()
    os.chmod(path, 0o600)
    return connection


def stage_values(stage):
    return [
        stage.get("feature", ""),
        stage["scope_fingerprint"],
        stage.get("benchmark_kind", "unknown"),
        stage.get("provider"),
        stage.get("model"),
        stage.get("thinking"),
        json.dumps(stage.get("task_ids", []), separators=(",", ":")),
        stage.get("binary_sha256"),
        stage.get("git_commit"),
        stage.get("report_path"),
        stage.get("notes"),
        stage.get("created_at"),
    ]


def sync_stage(connection, stage):
    values = stage_values(stage)
    inserted = connection.execute(
        """
        INSERT OR IGNORE INTO benchmark_stages
            (id, feature, scope_fingerprint, benchmark_kind, provider, model,
             thinking, task_ids_json, binary_sha256, git_commit, report_path,
             notes, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        [stage["id"], *values],
    )
    row = connection.execute(
        """
        SELECT feature, scope_fingerprint, benchmark_kind, provider, model,
               thinking, task_ids_json, binary_sha256, git_commit, report_path,
               notes, created_at
        FROM benchmark_stages
        WHERE id = ?
        """,
        (stage["id"],),
    ).fetchone()
    if list(row) != values:
        raise ValueError(f"stage {stage['id']} is immutable and differs from the database")
    return inserted.rowcount == 1


def load_task_summaries(run_dir):
    summaries = []
    for path in sorted(Path(run_dir).glob("*.json")):
        try:
            summary = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if summary.get("task_id"):
            summaries.append(summary)
    return summaries


def numeric(summary, field):
    value = summary.get(field)
    return float(value) if isinstance(value, (int, float)) else 0.0


def sync_run(connection, run_id, results_dir):
    run_dir = Path(results_dir) / run_id
    summaries = load_task_summaries(run_dir)
    if not summaries:
        raise ValueError(f"run directory has no task summaries: {run_dir}")
    costs = [summary.get("cost_usd") for summary in summaries]
    cost = (
        sum(float(value) for value in costs)
        if all(value is not None for value in costs)
        else None
    )
    run_values = [
        str(run_dir.resolve()),
        min((summary.get("started_at") or "" for summary in summaries), default=""),
        max((summary.get("ended_at") or "" for summary in summaries), default=""),
        len(summaries),
        sum(numeric(summary, "total_tokens") for summary in summaries),
        sum(numeric(summary, "prompt_tokens") for summary in summaries),
        sum(numeric(summary, "input") for summary in summaries),
        sum(numeric(summary, "cache_write") for summary in summaries),
        sum(numeric(summary, "cache_read") for summary in summaries),
        sum(numeric(summary, "output") for summary in summaries),
        sum(numeric(summary, "llm_calls") for summary in summaries),
        cost,
        sum(numeric(summary, "wall_ms") for summary in summaries),
        sum(summary.get("correct") is True for summary in summaries),
    ]
    connection.execute(
        """
        INSERT OR REPLACE INTO benchmark_runs
            (id, run_dir, started_at, ended_at, task_count, total_tokens,
             prompt_tokens, fresh_input_tokens, cache_write_tokens,
             cache_read_tokens, output_tokens, llm_calls, cost_usd, wall_ms,
             correct_tasks)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        [run_id, *run_values],
    )
    for summary in summaries:
        connection.execute(
            """
            INSERT OR REPLACE INTO task_results
                (run_id, task_id, task_name, total_tokens, prompt_tokens,
                 fresh_input_tokens, cache_write_tokens, cache_read_tokens,
                 output_tokens, llm_calls, cost_usd, wall_ms, correct,
                 checks_passed, checks_total, context_components_json, raw_json)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                run_id,
                summary["task_id"],
                summary.get("task_name"),
                numeric(summary, "total_tokens"),
                numeric(summary, "prompt_tokens"),
                numeric(summary, "input"),
                numeric(summary, "cache_write"),
                numeric(summary, "cache_read"),
                numeric(summary, "output"),
                numeric(summary, "llm_calls"),
                summary.get("cost_usd"),
                numeric(summary, "wall_ms"),
                summary.get("correct"),
                summary.get("checks_passed"),
                summary.get("checks_total"),
                json.dumps(
                    summary.get("context_component_tokens_est_total"),
                    separators=(",", ":"),
                ),
                json.dumps(summary, separators=(",", ":"), sort_keys=True),
            ),
        )


def sync_manifest(connection, manifest_path, results_dir):
    manifest = json.loads(Path(manifest_path).read_text(encoding="utf-8"))
    if manifest.get("schema_version") != 1:
        raise ValueError("unsupported optimization lineage schema")
    stages = manifest.get("stages") or []
    stage_ids = [stage["id"] for stage in stages]
    if len(stage_ids) != len(set(stage_ids)):
        raise ValueError("optimization lineage contains duplicate stage ids")
    inserted_stages = set()
    for stage in stages:
        if sync_stage(connection, stage):
            inserted_stages.add(stage["id"])
    for stage in stages:
        parent = stage.get("parent")
        current_parent = connection.execute(
            "SELECT parent_id FROM benchmark_stages WHERE id = ?",
            (stage["id"],),
        ).fetchone()[0]
        if stage["id"] not in inserted_stages and current_parent != parent:
            raise ValueError(
                f"stage {stage['id']} parent is immutable: "
                f"database={current_parent}, manifest={parent}"
            )
        connection.execute(
            "UPDATE benchmark_stages SET parent_id = ? WHERE id = ?",
            (parent, stage["id"]),
        )
        for run_id in stage.get("run_dirs", []):
            sync_run(connection, run_id, results_dir)
            connection.execute(
                "INSERT OR IGNORE INTO stage_runs(stage_id, run_id) VALUES (?, ?)",
                (stage["id"], run_id),
            )
        for metric, specification in (stage.get("metrics") or {}).items():
            value, unit = specification
            connection.execute(
                """
                INSERT OR REPLACE INTO stage_metrics(stage_id, metric, scope, value, unit)
                VALUES (?, ?, 'aggregate', ?, ?)
                """,
                (stage["id"], metric, value, unit),
            )
        for artifact in stage.get("artifacts", []):
            connection.execute(
                """
                INSERT OR REPLACE INTO benchmark_artifacts(stage_id, path, sha256, kind)
                VALUES (?, ?, ?, ?)
                """,
                (
                    stage["id"],
                    artifact["path"],
                    artifact.get("sha256"),
                    artifact.get("kind"),
                ),
            )
    connection.commit()


def stage_task_ids(connection, stage_id):
    row = connection.execute(
        "SELECT scope_fingerprint, task_ids_json FROM benchmark_stages WHERE id = ?",
        (stage_id,),
    ).fetchone()
    if not row:
        raise ValueError(f"unknown stage: {stage_id}")
    return row[0], json.loads(row[1])


def stage_results(connection, stage_id, task_ids):
    rows = connection.execute(
        """
        SELECT tr.run_id, tr.task_id, tr.total_tokens, tr.cost_usd, tr.wall_ms
        FROM task_results tr
        JOIN stage_runs sr ON sr.run_id = tr.run_id
        WHERE sr.stage_id = ?
        """,
        (stage_id,),
    ).fetchall()
    if task_ids:
        allowed = set(task_ids)
        rows = [row for row in rows if row[1] in allowed]
    by_run = defaultdict(list)
    by_task = defaultdict(list)
    for run_id, task_id, tokens, cost, wall in rows:
        by_run[run_id].append((tokens, cost, wall))
        by_task[task_id].append((tokens, cost, wall))
    run_totals = []
    for values in by_run.values():
        costs = [value[1] for value in values]
        run_totals.append(
            (
                sum(value[0] for value in values),
                sum(costs) if all(cost is not None for cost in costs) else None,
                sum(value[2] for value in values),
            )
        )
    return run_totals, by_task


def mean(values):
    return sum(values) / len(values) if values else None


def delta(baseline, candidate):
    return round((candidate / baseline - 1.0) * 100.0, 10) if baseline else None


def compare_stages(connection, baseline_id, candidate_id):
    baseline_scope, baseline_tasks = stage_task_ids(connection, baseline_id)
    candidate_scope, candidate_tasks = stage_task_ids(connection, candidate_id)
    if baseline_scope != candidate_scope:
        raise ValueError(
            "scope fingerprints differ: "
            f"{baseline_id}={baseline_scope}, {candidate_id}={candidate_scope}"
        )
    if baseline_tasks != candidate_tasks:
        raise ValueError("task-set fingerprints differ")
    baseline_runs, baseline_by_task = stage_results(
        connection, baseline_id, baseline_tasks
    )
    candidate_runs, candidate_by_task = stage_results(
        connection, candidate_id, candidate_tasks
    )
    if not baseline_runs or not candidate_runs:
        raise ValueError("both stages need imported raw runs")

    def metric(index):
        baseline_values = [run[index] for run in baseline_runs if run[index] is not None]
        candidate_values = [run[index] for run in candidate_runs if run[index] is not None]
        baseline_value = mean(baseline_values)
        candidate_value = mean(candidate_values)
        return {
            "baseline": baseline_value,
            "candidate": candidate_value,
            "delta_pct": delta(baseline_value, candidate_value),
        }

    per_task = {}
    for task_id in baseline_tasks:
        baseline_values = baseline_by_task.get(task_id, [])
        candidate_values = candidate_by_task.get(task_id, [])
        baseline_value = mean([value[0] for value in baseline_values])
        candidate_value = mean([value[0] for value in candidate_values])
        baseline_cost = mean(
            [value[1] for value in baseline_values if value[1] is not None]
        )
        candidate_cost = mean(
            [value[1] for value in candidate_values if value[1] is not None]
        )
        baseline_wall = mean([value[2] for value in baseline_values])
        candidate_wall = mean([value[2] for value in candidate_values])
        per_task[task_id] = {
            "baseline": baseline_value,
            "candidate": candidate_value,
            "delta_pct": delta(baseline_value, candidate_value),
            "cost_usd": {
                "baseline": baseline_cost,
                "candidate": candidate_cost,
                "delta_pct": delta(baseline_cost, candidate_cost),
            },
            "wall_ms": {
                "baseline": baseline_wall,
                "candidate": candidate_wall,
                "delta_pct": delta(baseline_wall, candidate_wall),
            },
        }
    return {
        "baseline_stage": baseline_id,
        "candidate_stage": candidate_id,
        "scope_fingerprint": baseline_scope,
        "total_tokens": metric(0),
        "cost_usd": metric(1),
        "wall_ms": metric(2),
        "per_task": per_task,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--db",
        default=str(Path.home() / ".daimonos" / "benchmarks.db"),
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    sync_parser = subparsers.add_parser("sync")
    sync_parser.add_argument("--manifest", required=True)
    sync_parser.add_argument("--results-dir", required=True)

    compare_parser = subparsers.add_parser("compare")
    compare_parser.add_argument("baseline")
    compare_parser.add_argument("candidate")
    compare_parser.add_argument("--json", action="store_true")

    subparsers.add_parser("list")
    args = parser.parse_args(argv)

    try:
        with connect(args.db) as connection:
            if args.command == "sync":
                sync_manifest(connection, args.manifest, args.results_dir)
                print(f"Synced benchmark lineage to {Path(args.db).expanduser()}")
            elif args.command == "compare":
                result = compare_stages(connection, args.baseline, args.candidate)
                if args.json:
                    json.dump(result, sys.stdout, indent=2)
                    sys.stdout.write("\n")
                else:
                    def formatted_delta(metric):
                        value = result[metric]["delta_pct"]
                        return "n/a" if value is None else f"{value:+.2f}%"

                    print(
                        f"{args.baseline} -> {args.candidate}: "
                        f"tokens {formatted_delta('total_tokens')}, "
                        f"cost {formatted_delta('cost_usd')}, "
                        f"wall {formatted_delta('wall_ms')}"
                    )
                    for task_id, values in result["per_task"].items():
                        token_delta = values["delta_pct"]
                        rendered = "n/a" if token_delta is None else f"{token_delta:+.2f}%"
                        print(f"  {task_id}: {rendered} tokens")
            else:
                for row in connection.execute(
                    """
                    SELECT id, parent_id, scope_fingerprint, feature
                    FROM benchmark_stages
                    ORDER BY rowid
                    """
                ):
                    print("\t".join("" if value is None else str(value) for value in row))
    except (OSError, ValueError, sqlite3.Error) as error:
        print(f"benchmark_db: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
