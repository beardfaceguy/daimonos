#!/usr/bin/env python3
"""Compare two server-bench result directories and print Δ per metric.

Usage:
    python3 compare.py results/baseline/ results/candidate/

Reports, per task, the percent change in median, p99, and stdev of
per-call timing, plus the absolute change in end-of-run resource
snapshots (RSS, FD count, inotify watches).

The threshold for "significant" Δ is intentionally simple: anything
outside ±5% on median or ±20% on stdev gets a marker. Real
significance testing would need confidence intervals from many bench
runs, which is what option A in the original Vikunja #264 ticket
covers — this script is meant to be the first-line filter.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

SIGNIFICANT_MEDIAN_PCT = 5.0
SIGNIFICANT_STDEV_PCT = 20.0


def load_results(path: Path) -> dict:
    f = path / "results.json"
    if not f.exists():
        raise SystemExit(f"no results.json in {path}")
    return json.loads(f.read_text())


def pct(new: float, old: float) -> float:
    if old == 0:
        return float("inf") if new != 0 else 0.0
    return (new - old) / old * 100.0


def fmt_pct(value: float, threshold: float) -> str:
    marker = ""
    if abs(value) >= threshold:
        marker = "  <<<" if value > 0 else "  >>>"
    sign = "+" if value > 0 else ""
    return f"{sign}{value:5.1f}%{marker}"


def fmt_ns(ns: float) -> str:
    if ns < 1_000:
        return f"{ns:.0f}ns"
    if ns < 1_000_000:
        return f"{ns/1000:.1f}µs"
    if ns < 1_000_000_000:
        return f"{ns/1_000_000:.2f}ms"
    return f"{ns/1_000_000_000:.2f}s"


def compare_task(a: dict, b: dict) -> None:
    sa = a["summary"]
    sb = b["summary"]
    print(f"  {'metric':<14} {'baseline':>14} {'candidate':>14} {'delta':>14}")
    rows = [
        ("count", sa["count"], sb["count"], lambda x: str(int(x)), 0.0),
        ("median", sa["median_ns"], sb["median_ns"], fmt_ns, SIGNIFICANT_MEDIAN_PCT),
        ("p95", sa["p95_ns"], sb["p95_ns"], fmt_ns, SIGNIFICANT_MEDIAN_PCT),
        ("p99", sa["p99_ns"], sb["p99_ns"], fmt_ns, SIGNIFICANT_MEDIAN_PCT),
        ("stdev", sa["stdev_ns"], sb["stdev_ns"], fmt_ns, SIGNIFICANT_STDEV_PCT),
        ("max", sa["max_ns"], sb["max_ns"], fmt_ns, SIGNIFICANT_MEDIAN_PCT),
    ]
    for name, av, bv, fmt, threshold in rows:
        delta_str = "—" if threshold == 0.0 else fmt_pct(pct(bv, av), threshold)
        print(f"  {name:<14} {fmt(av):>14} {fmt(bv):>14} {delta_str:>14}")

    ra = a.get("resources", {})
    rb = b.get("resources", {})
    res_rows = [
        ("rss_kb", "VmRSS (KB)"),
        ("fd_count", "FD count"),
        ("inotify_watches", "inotify"),
    ]
    print("  --- resources (end-of-run snapshot) ---")
    for key, label in res_rows:
        av = ra.get(key, -1)
        bv = rb.get(key, -1)
        if av < 0 and bv < 0:
            continue
        delta = bv - av if av >= 0 and bv >= 0 else "—"
        print(f"  {label:<14} {av:>14} {bv:>14} {str(delta):>14}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    args = parser.parse_args()

    a = load_results(args.baseline)
    b = load_results(args.candidate)
    a_tasks = {t["task_id"]: t for t in a["tasks"]}
    b_tasks = {t["task_id"]: t for t in b["tasks"]}

    print(f"baseline:  {args.baseline} ({a['replicates']} replicates)")
    print(f"candidate: {args.candidate} ({b['replicates']} replicates)")
    print()

    only_a = sorted(set(a_tasks) - set(b_tasks))
    only_b = sorted(set(b_tasks) - set(a_tasks))
    if only_a:
        print(f"tasks only in baseline:  {only_a}")
    if only_b:
        print(f"tasks only in candidate: {only_b}")

    for tid in sorted(set(a_tasks) & set(b_tasks)):
        print(f"\n=== {tid} — {a_tasks[tid].get('description', '')} ===")
        compare_task(a_tasks[tid], b_tasks[tid])
    return 0


if __name__ == "__main__":
    sys.exit(main())
