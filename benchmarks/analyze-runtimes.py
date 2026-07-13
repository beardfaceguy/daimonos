#!/usr/bin/env python3
"""Compare token usage across agent runtimes (daimonos / claude / cursor).

Reads the per-task summary JSONs written by run-runtime-benchmark.sh and reports
normalized token usage per runtime, correctness-gated (runs whose checks failed
are excluded from the token aggregates so a runtime can't "win" by doing less).

Usage:
  analyze-runtimes.py <results-dir> [tag-substring]

Metric schema (all runtimes normalized to this):
  input        fresh, non-cached prompt tokens
  cache_write  tokens written to the prompt cache
  cache_read   tokens served from the prompt cache
  output       generated tokens
  total_tokens input + cache_write + cache_read + output
  cost_usd     dollars (daimonos via OpenRouter often 0; cursor joined from the
               admin CSV by cursor-attribute.py; costs are NOT provider-neutral)
"""
import json
import os
import sys
from collections import defaultdict

TOKEN_FIELDS = ["input", "cache_write", "cache_read", "output", "total_tokens"]
RUNTIME_ORDER = ["daimonos", "claude", "cursor"]
UPSTREAM = {
    "daimonos": "OpenRouter",
    "claude": "Anthropic direct",
    "cursor": "Cursor",
}


def load_summaries(results_dir, tag):
    """Yield task summary dicts from run dirs, optionally filtered by tag."""
    for run_dir in sorted(os.listdir(results_dir)):
        full = os.path.join(results_dir, run_dir)
        if not os.path.isdir(full):
            continue
        if tag and tag not in run_dir:
            continue
        for fn in sorted(os.listdir(full)):
            if not fn.endswith(".json"):
                continue
            # skip the raw token-log delta (*.tokenlog.jsonl is not .json anyway)
            path = os.path.join(full, fn)
            try:
                with open(path) as f:
                    s = json.load(f)
            except (ValueError, OSError):
                continue
            if "runtime" in s and "task_id" in s:
                yield s


def gated(summary):
    """A run counts toward token aggregates unless its checks explicitly failed."""
    if summary.get("is_error"):
        return False
    # correct is True (passed), False (failed), or None (no machine checks)
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
        print(f"No summaries found under {results_dir}" + (f" (tag '{tag}')" if tag else ""))
        sys.exit(1)

    # per runtime: aggregate token sums over gated runs; per task: list of totals
    agg = {rt: defaultdict(float) for rt in RUNTIME_ORDER}
    attempts = defaultdict(int)
    correct = defaultdict(int)
    cost_known = {rt: True for rt in RUNTIME_ORDER}
    per_task = defaultdict(lambda: defaultdict(list))  # task_id -> runtime -> [total]
    models = defaultdict(set)

    for s in summaries:
        rt = s["runtime"]
        if rt not in agg:
            continue
        attempts[rt] += 1
        models[rt].add(s.get("model_slug", "?"))
        if s.get("correct") is True:
            correct[rt] += 1
        if not gated(s):
            continue
        for fld in TOKEN_FIELDS:
            agg[rt][fld] += s.get(fld, 0) or 0
        c = s.get("cost_usd", None)
        if c is None:
            cost_known[rt] = False
        else:
            try:
                agg[rt]["cost_usd"] += float(c)
            except (TypeError, ValueError):
                cost_known[rt] = False
        per_task[s["task_id"]][rt].append(s.get("total_tokens", 0) or 0)

    present = [rt for rt in RUNTIME_ORDER if attempts[rt] > 0]

    print("=" * 78)
    print("Runtime token benchmark")
    print("=" * 78)
    for rt in present:
        ms = ", ".join(sorted(models[rt]))
        print(f"  {rt:9s} model={ms}  upstream={UPSTREAM[rt]}  "
              f"runs={attempts[rt]} correct={correct[rt]}")
    print("")

    # Per-runtime aggregate table
    hdr = f"{'runtime':9s} {'gated':>6s} " + " ".join(f"{f:>13s}" for f in TOKEN_FIELDS) + f" {'cost_usd':>10s}"
    print(hdr)
    print("-" * len(hdr))
    for rt in present:
        gated_runs = sum(1 for s in summaries if s["runtime"] == rt and gated(s))
        row = f"{rt:9s} {gated_runs:>6d} "
        row += " ".join(f"{fmt(int(agg[rt][f])):>13s}" for f in TOKEN_FIELDS)
        cost = f"${agg[rt]['cost_usd']:.4f}" if cost_known[rt] else "n/a"
        row += f" {cost:>10s}"
        print(row)
    print("")

    # Per-task total-token comparison (mean across runs)
    print("Per-task total tokens (mean over gated runs)")
    thdr = f"{'task':22s} " + " ".join(f"{rt:>12s}" for rt in present)
    print(thdr)
    print("-" * len(thdr))
    for task_id in sorted(per_task):
        row = f"{task_id:22s} "
        for rt in present:
            vals = per_task[task_id][rt]
            row += f"{fmt(int(sum(vals) / len(vals))) if vals else '-':>12s} "
        print(row.rstrip())
    print("")

    # Headline: total tokens relative to daimonos, if present
    if "daimonos" in present:
        base = agg["daimonos"]["total_tokens"]
        print("Total-token comparison (vs daimonos):")
        for rt in present:
            t = agg[rt]["total_tokens"]
            if rt == "daimonos" or base == 0:
                print(f"  {rt:9s} {fmt(int(t)):>13s}")
            else:
                delta = (t - base) / base * 100.0
                sign = "+" if delta >= 0 else ""
                print(f"  {rt:9s} {fmt(int(t)):>13s}  ({sign}{delta:.1f}% vs daimonos)")
    print("")
    print("Note: costs are NOT provider-neutral (daimonos=OpenRouter, "
          "claude=Anthropic, cursor=Cursor). Compare token counts for "
          "efficiency; treat cost as provider-priced.")


if __name__ == "__main__":
    main()
