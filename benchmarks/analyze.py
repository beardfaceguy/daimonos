#!/usr/bin/env python3
"""Aggregate and compare daimonos agent-mode benchmark runs, grouped by model.

Reads the per-task summary JSONs written by `bench-agent.sh` (one dir per run
under `results/`) and reports correctness-gated token/cost aggregates per model,
plus a per-task mean-token table across every run of each model. Because it
groups by the `model_slug` stamped into each summary, running the suite on
several models (as they change over time) and pointing this at `results/` gives
a direct cross-model comparison — no hardcoded model list or prices.

Usage:
  analyze.py <results-dir> [tag-substring]

  tag-substring   Optional: only include run dirs whose name contains it.

Gating: a task counts toward token/cost aggregates unless it errored or its
machine checks explicitly failed (correct == False). Tasks with no checks
(correct == None) still count — they produced output, we just can't grade it.
This stops a model from "winning" on tokens by doing less work and failing.

Cost: taken from each summary's `cost_usd`, which daimonos logs from the
provider (OpenRouter). No prices are hardcoded here, so this stays correct as
models and pricing change.
"""

import json
import os
import sys
from collections import defaultdict

TOKEN_FIELDS = ["input", "cache_write", "cache_read", "output", "total_tokens"]


def load_summaries(results_dir, tag):
    """Yield per-task summary dicts from run dirs, optionally filtered by tag."""
    for run_dir in sorted(os.listdir(results_dir)):
        full = os.path.join(results_dir, run_dir)
        if not os.path.isdir(full):
            continue
        if tag and tag not in run_dir:
            continue
        for fn in sorted(os.listdir(full)):
            if not fn.endswith(".json"):
                continue
            path = os.path.join(full, fn)
            try:
                with open(path) as f:
                    s = json.load(f)
            except (ValueError, OSError):
                continue
            if "task_id" in s and ("model_slug" in s or "model" in s):
                yield s


def model_of(summary):
    return summary.get("model_slug") or summary.get("model") or "?"


def gated(summary):
    """Counts toward aggregates unless it errored or its checks failed."""
    if summary.get("is_error"):
        return False
    return summary.get("correct") is not False


def fmt(n):
    return f"{n:,}" if isinstance(n, (int, float)) else str(n)


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    results_dir = sys.argv[1]
    tag = sys.argv[2] if len(sys.argv) > 2 else ""

    summaries = list(load_summaries(results_dir, tag))
    if not summaries:
        where = f" (tag '{tag}')" if tag else ""
        print(f"No summaries found under {results_dir}{where}")
        sys.exit(1)

    agg = defaultdict(lambda: defaultdict(float))   # model -> field -> sum
    attempts = defaultdict(int)
    correct = defaultdict(int)
    checked = defaultdict(int)
    gated_runs = defaultdict(int)
    cost_known = defaultdict(lambda: True)
    per_task = defaultdict(lambda: defaultdict(list))  # task_id -> model -> [total]

    for s in summaries:
        model = model_of(s)
        attempts[model] += 1
        if s.get("correct") is True:
            correct[model] += 1
        if s.get("correct") in (True, False):
            checked[model] += 1
        if not gated(s):
            continue
        gated_runs[model] += 1
        for fld in TOKEN_FIELDS:
            agg[model][fld] += s.get(fld, 0) or 0
        c = s.get("cost_usd", None)
        if c is None:
            cost_known[model] = False
        else:
            try:
                agg[model]["cost_usd"] += float(c)
            except (TypeError, ValueError):
                cost_known[model] = False
        per_task[s["task_id"]][model].append(s.get("total_tokens", 0) or 0)

    models = sorted(attempts)

    print("=" * 78)
    print("daimonos agent-mode benchmark")
    print("=" * 78)
    for m in models:
        grade = f"{correct[m]}/{checked[m]} correct" if checked[m] else "no graded checks"
        print(f"  {m}: runs={attempts[m]}  gated={gated_runs[m]}  {grade}")
    print()

    hdr = f"{'model':32s} {'gated':>5s} " + " ".join(f"{f:>12s}" for f in TOKEN_FIELDS) + f" {'cost_usd':>10s}"
    print(hdr)
    print("-" * len(hdr))
    for m in models:
        row = f"{m:32s} {gated_runs[m]:>5d} "
        row += " ".join(f"{fmt(int(agg[m][f])):>12s}" for f in TOKEN_FIELDS)
        cost = f"${agg[m]['cost_usd']:.4f}" if cost_known[m] else "n/a"
        row += f" {cost:>10s}"
        print(row)
    print()

    # Per-task mean total tokens, one column per model (mean over gated runs).
    print("Per-task total tokens (mean over gated runs)")
    thdr = f"{'task':22s} " + " ".join(f"{m[:16]:>16s}" for m in models)
    print(thdr)
    print("-" * len(thdr))
    for task_id in sorted(per_task):
        row = f"{task_id:22s} "
        for m in models:
            vals = per_task[task_id][m]
            row += f"{(fmt(int(sum(vals) / len(vals))) if vals else '-'):>16s} "
        print(row.rstrip())
    print()

    if len(models) > 1:
        base = models[0]
        base_total = agg[base]["total_tokens"]
        print(f"Total-token comparison (vs {base}):")
        for m in models:
            t = agg[m]["total_tokens"]
            if m == base or base_total == 0:
                print(f"  {m:32s} {fmt(int(t)):>12s}")
            else:
                delta = (t - base_total) / base_total * 100.0
                sign = "+" if delta >= 0 else ""
                print(f"  {m:32s} {fmt(int(t)):>12s}  ({sign}{delta:.1f}% vs {base})")


if __name__ == "__main__":
    main()
