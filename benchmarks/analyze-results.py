#!/usr/bin/env python3
"""Analyze benchmark results across arms (baseline, baseline-terse, daimonos).

Aggregates every matching run per arm — including BENCH_RUNS-produced
-r1..-rN directories — and reports per-task means with min/max spread, so a
delta can be read against its run-to-run noise instead of a single sample.

Usage:
  python3 analyze-results.py <results-dir> [tag]

With a tag, only runs whose name matches <arm>-<tag> (or <arm>-<tag>-rN) are
included; without one, only untagged runs (<arm> or <arm>-rN).
"""

import json
import re
import sys
from pathlib import Path

ARMS = ["baseline", "baseline-terse", "daimonos", "daimonos-terse"]
ARM_ALIASES = {"baseline": ["baseline", "cursor"]}
NUMERIC_METRICS = [
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "tool_calls",
    "mcp_tool_calls",
    "cost_usd",
    "wall_ms",
]


def load_run(run_dir: Path) -> dict:
    """Load all task summaries from one run directory."""
    results = {}
    for f in sorted(run_dir.glob("*.json")):
        if f.name.endswith(".raw.jsonl"):
            continue
        data = json.loads(f.read_text())
        results[data["task_id"]] = data
    return results


def find_arm_runs(results_dir: Path, arm: str, tag: str = "") -> list[Path]:
    """All run dirs belonging to (arm, tag), including -rN repetitions."""
    aliases = ARM_ALIASES.get(arm, [arm])
    runs = []
    for d in sorted(Path(results_dir).iterdir()):
        if not d.is_dir():
            continue
        parts = d.name.split("-", 2)  # <date>-<time>-<rest>
        if len(parts) < 3:
            continue
        rest = parts[2]
        for alias in aliases:
            base = f"{alias}-{tag}" if tag else alias
            if rest == base or re.fullmatch(re.escape(base) + r"-r\d+", rest):
                runs.append(d)
                break
    return runs


def aggregate(run_dirs: list[Path]) -> dict:
    """Per-task cross-run statistics: mean/min/max per metric, success rate,
    contaminated-run count, and n (runs that included the task)."""
    per_task: dict[str, list[dict]] = {}
    for rd in run_dirs:
        for task_id, data in load_run(rd).items():
            per_task.setdefault(task_id, []).append(data)

    stats = {}
    for task_id, rows in per_task.items():
        entry: dict = {"n": len(rows), "task_name": rows[0].get("task_name", task_id)}
        # Correctness gate (#929): runs whose checks failed must not shape the
        # metric stats — savings on a broken run don't count. `correct` absent
        # or null (pre-gate data, or tasks without checks) counts as correct.
        correct_rows = [r for r in rows if r.get("correct") is not False]
        entry["correct_rate"] = len(correct_rows) / len(rows)
        metric_rows = correct_rows or rows  # all-failed: report, flagged by rate=0
        for metric in NUMERIC_METRICS:
            vals = [row.get(metric) or 0 for row in metric_rows]
            entry[metric] = {
                "mean": sum(vals) / len(vals),
                "min": min(vals),
                "max": max(vals),
            }
        entry["success_rate"] = sum(1 for r in rows if r.get("success")) / len(rows)
        entry["contaminated_runs"] = sum(1 for r in rows if r.get("contaminated"))
        stats[task_id] = entry
    return stats


def _fmt_spread(m: dict, width: int = 0) -> str:
    """mean with min–max spread when runs disagree, e.g. `210 (180–240)`."""
    mean, lo, hi = m["mean"], m["min"], m["max"]
    s = f"{mean:,.0f}"
    if hi != lo:
        s += f" ({lo:,.0f}–{hi:,.0f})"
    return s.rjust(width) if width else s


def print_report(arm_stats: dict[str, dict]):
    """arm_stats: arm name -> aggregate() output (only arms with data)."""
    all_tasks = sorted({t for s in arm_stats.values() for t in s})

    print("\n" + "=" * 118)
    print(f"{'Task':<32} {'Arm':<15} {'n':>2} {'Out Tok':>20} {'Tools':>14} {'Cost $':>16} {'Wall ms':>14} {'OK|✓':>7}")
    print("=" * 118)
    for task_id in all_tasks:
        for arm, stats in arm_stats.items():
            t = stats.get(task_id)
            if not t:
                continue
            ok = f"{t['success_rate'] * 100:.0f}|{t.get('correct_rate', 1.0) * 100:.0f}"
            if t["contaminated_runs"]:
                ok += "!"
            cost = t["cost_usd"]
            cost_s = f"{cost['mean']:.4f}"
            if cost["max"] != cost["min"]:
                cost_s += f" ±{(cost['max'] - cost['min']) / 2:.4f}"
            print(
                f"{t['task_name'][:32]:<32} {arm:<15} {t['n']:>2}"
                f" {_fmt_spread(t['output_tokens'], 20)}"
                f" {_fmt_spread(t['tool_calls'], 14)}"
                f" {cost_s:>16}"
                f" {_fmt_spread(t['wall_ms'], 14)}"
                f" {ok:>7}"
            )
        print("-" * 118)

    # Safety surfacing must not depend on how many arms were analyzed — a
    # single-arm run with contaminated results still has to warn loudly.
    _print_warnings(arm_stats)

    # Aggregate over tasks present in every arm, so sums are comparable.
    shared = sorted(set.intersection(*(set(s) for s in arm_stats.values())))
    if not shared or len(arm_stats) < 2:
        print("\nNo shared tasks across arms; skipping aggregate comparison.")
        return

    def totals(stats):
        return {m: sum(stats[t][m]["mean"] for t in shared) for m in NUMERIC_METRICS}

    arm_totals = {arm: totals(stats) for arm, stats in arm_stats.items()}

    print("\n" + "=" * 96)
    print(f"AGGREGATE — mean per run, summed over {len(shared)} shared task(s)")
    print("=" * 96)
    header = f"{'Metric':<22}" + "".join(f"{arm:>18}" for arm in arm_totals)
    print(header)
    print("-" * 96)
    for metric in ["output_tokens", "cache_read_tokens", "tool_calls", "cost_usd", "wall_ms"]:
        row = f"{metric:<22}"
        for arm in arm_totals:
            v = arm_totals[arm][metric]
            row += f"{v:>18,.4f}" if metric == "cost_usd" else f"{v:>18,.0f}"
        print(row)

    # Report deltas for each daimonos* arm (Full and the verbosity-dialed
    # daimonos-terse) against every other arm, so both the tools-only effect and
    # the prefix-diet lever delta (daimonos-terse vs daimonos) are visible.
    subjects = [a for a in arm_totals if a.startswith("daimonos")]
    if subjects:
        print()
        for subj in subjects:
            d = arm_totals[subj]
            for ref_arm in arm_totals:
                if ref_arm == subj:
                    continue
                r = arm_totals[ref_arm]
                deltas = []
                for metric, label in [("output_tokens", "out tok"), ("cost_usd", "cost"), ("wall_ms", "wall")]:
                    if r[metric]:
                        pct = (d[metric] - r[metric]) / r[metric] * 100
                        deltas.append(f"{label} {pct:+.1f}%")
                print(f"{subj} vs {ref_arm}: " + ", ".join(deltas))
        if "daimonos-terse" in arm_totals and "baseline-terse" in arm_totals:
            print(
                "(daimonos-terse vs baseline-terse: tools-only effect WITH the prefix-diet"
                " levers applied; daimonos-terse vs daimonos: the lever delta itself.)"
            )
        elif "daimonos" in arm_totals and "baseline-terse" in arm_totals:
            print("(daimonos vs baseline-terse isolates the tools-only effect; vs baseline includes the prompt.)")


def _print_warnings(arm_stats: dict):
    for arm, stats in arm_stats.items():
        contaminated = sum(t["contaminated_runs"] for t in stats.values())
        if contaminated:
            print(f"\nWARNING: {contaminated} contaminated run(s) in {arm} — isolation failed; numbers unreliable.")

    for arm, stats in arm_stats.items():
        failed_tasks = [t["task_name"] for t in stats.values() if t.get("correct_rate", 1.0) < 1.0]
        if failed_tasks:
            print(
                f"\nNOTE: {arm} had runs that failed correctness checks on: "
                + ", ".join(sorted(failed_tasks))
                + " — those runs are excluded from metric stats (OK|✓ column shows rates)."
            )


def main():
    if len(sys.argv) < 2:
        print("Usage: python3 analyze-results.py <results-dir> [tag]")
        sys.exit(1)

    results_dir = Path(sys.argv[1])
    tag = sys.argv[2] if len(sys.argv) >= 3 else ""

    arm_stats = {}
    for arm in ARMS:
        runs = find_arm_runs(results_dir, arm, tag)
        if runs:
            arm_stats[arm] = aggregate(runs)
            print(f"{arm}: {len(runs)} run(s) — {', '.join(r.name for r in runs)}")

    if not arm_stats:
        print("No results found. Run benchmarks first, e.g.:")
        print("  BENCH_RUNS=4 ./run-benchmark.sh baseline")
        print("  BENCH_RUNS=4 ./run-benchmark.sh baseline-terse")
        print("  BENCH_RUNS=4 ./run-benchmark.sh daimonos")
        sys.exit(1)

    print_report(arm_stats)


if __name__ == "__main__":
    main()
