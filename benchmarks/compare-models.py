#!/usr/bin/env python3
"""Cross-model comparison of daimonos benchmark results."""

import json
import sys
from pathlib import Path


def load_run(run_dir: Path) -> dict:
    results = {}
    for f in sorted(run_dir.glob("*.json")):
        if f.name.endswith(".raw.jsonl"):
            continue
        data = json.loads(f.read_text())
        results[data["task_id"]] = data
    return results


def find_tagged_run(results_dir: Path, mode: str, tag: str) -> Path | None:
    for d in sorted(results_dir.iterdir()):
        if not d.is_dir():
            continue
        parts = d.name.split("-", 2)
        if len(parts) >= 3 and parts[2] == f"{mode}-{tag}":
            return d
    return None


def main():
    results_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("results")
    tags = sys.argv[2:] if len(sys.argv) > 2 else []

    if not tags:
        seen = set()
        for d in sorted(results_dir.iterdir()):
            if not d.is_dir():
                continue
            parts = d.name.split("-", 2)
            if len(parts) >= 3:
                rest = parts[2]
                if rest.startswith("cursor-"):
                    seen.add(rest.removeprefix("cursor-"))
                elif rest.startswith("daimonos-"):
                    seen.add(rest.removeprefix("daimonos-"))
        tags = sorted(seen)

    if not tags:
        print("No tagged runs found. Run benchmarks with BENCH_TAG=<tag> first.")
        sys.exit(1)

    print(f"Models found: {', '.join(tags)}")
    print()

    model_data = {}
    for tag in tags:
        c_dir = find_tagged_run(results_dir, "cursor", tag)
        d_dir = find_tagged_run(results_dir, "daimonos", tag)
        c = load_run(c_dir) if c_dir else {}
        d = load_run(d_dir) if d_dir else {}
        shared = sorted(set(c.keys()) & set(d.keys()))

        c_out = sum(c[t]["output_tokens"] for t in shared)
        d_out = sum(d[t]["output_tokens"] for t in shared)
        c_cache = sum(c[t]["cache_read_tokens"] for t in shared)
        d_cache = sum(d[t]["cache_read_tokens"] for t in shared)
        c_tools = sum(c[t]["tool_calls"] for t in shared)
        d_tools = sum(d[t]["tool_calls"] for t in shared)
        c_wall = sum(c[t]["wall_ms"] for t in shared)
        d_wall = sum(d[t]["wall_ms"] for t in shared)
        c_cachew = sum(c[t]["cache_write_tokens"] for t in shared)
        d_cachew = sum(d[t]["cache_write_tokens"] for t in shared)

        c_cost = c_out * 15.0 + c_cache * 0.30 + c_cachew * 3.75
        d_cost = d_out * 15.0 + d_cache * 0.30 + d_cachew * 3.75

        model_data[tag] = {
            "shared_tasks": len(shared),
            "cursor": {"out": c_out, "cache_r": c_cache, "cache_w": c_cachew, "tools": c_tools, "wall": c_wall, "cost": c_cost},
            "daimonos": {"out": d_out, "cache_r": d_cache, "cache_w": d_cachew, "tools": d_tools, "wall": d_wall, "cost": d_cost},
            "tasks": {t: {"cursor": c[t], "daimonos": d[t]} for t in shared},
        }

    # Summary table
    print("=" * 100)
    print(f"{'Model':<15} {'Metric':<20} {'Cursor':>12} {'Daimonos':>12} {'Delta':>12} {'Change':>10}")
    print("=" * 100)

    for tag in tags:
        md = model_data[tag]
        c, d = md["cursor"], md["daimonos"]
        mtok = 1_000_000

        print(f"{tag:<15} {'Output tokens':<20} {c['out']:>12,} {d['out']:>12,} {d['out'] - c['out']:>+12,} {(d['out'] - c['out']) / c['out'] * 100 if c['out'] else 0:>+9.1f}%")
        print(f"{'':15} {'Cache read tokens':<20} {c['cache_r']:>12,} {d['cache_r']:>12,} {d['cache_r'] - c['cache_r']:>+12,} {(d['cache_r'] - c['cache_r']) / c['cache_r'] * 100 if c['cache_r'] else 0:>+9.1f}%")
        print(f"{'':15} {'Tool calls':<20} {c['tools']:>12,} {d['tools']:>12,} {d['tools'] - c['tools']:>+12,} {(d['tools'] - c['tools']) / c['tools'] * 100 if c['tools'] else 0:>+9.1f}%")
        print(f"{'':15} {'Wall time (ms)':<20} {c['wall']:>12,} {d['wall']:>12,} {d['wall'] - c['wall']:>+12,} {(d['wall'] - c['wall']) / c['wall'] * 100 if c['wall'] else 0:>+9.1f}%")
        print(f"{'':15} {'Est. cost ($)':<20} {c['cost']/mtok:>12.4f} {d['cost']/mtok:>12.4f} {(d['cost'] - c['cost'])/mtok:>+12.4f} {(d['cost'] - c['cost']) / c['cost'] * 100 if c['cost'] else 0:>+9.1f}%")
        print("-" * 100)

    # Winner summary
    print()
    print("VERDICT PER MODEL")
    print("-" * 60)
    for tag in tags:
        md = model_data[tag]
        c, d = md["cursor"], md["daimonos"]
        cost_diff = (d["cost"] - c["cost"]) / c["cost"] * 100 if c["cost"] else 0
        out_diff = (d["out"] - c["out"]) / c["out"] * 100 if c["out"] else 0
        cache_diff = (d["cache_r"] - c["cache_r"]) / c["cache_r"] * 100 if c["cache_r"] else 0

        if cost_diff < 0:
            verdict = f"DAIMONOS WINS — {abs(cost_diff):.1f}% cheaper"
        elif cost_diff > 10:
            verdict = f"CURSOR WINS — daimonos {cost_diff:.1f}% more expensive"
        else:
            verdict = f"ROUGHLY EVEN — {abs(cost_diff):.1f}% difference"

        print(f"  {tag:<12} {verdict}")
        print(f"               out: {out_diff:+.1f}%  cache_r: {cache_diff:+.1f}%  tools: {d['tools'] - c['tools']:+d}")
    print()

    # Per-task heat map
    print("PER-TASK OUTPUT TOKEN DELTA (% change, negative = daimonos cheaper)")
    print("-" * 90)
    print(f"{'Task':<40}", end="")
    for tag in tags:
        print(f" {tag:>15}", end="")
    print()
    print("-" * 90)

    all_tasks = set()
    for tag in tags:
        all_tasks.update(model_data[tag]["tasks"].keys())

    for task in sorted(all_tasks):
        task_name = ""
        for tag in tags:
            if task in model_data[tag]["tasks"]:
                task_name = model_data[tag]["tasks"][task]["cursor"]["task_name"][:40]
                break
        print(f"{task_name:<40}", end="")
        for tag in tags:
            if task in model_data[tag]["tasks"]:
                c = model_data[tag]["tasks"][task]["cursor"]
                d = model_data[tag]["tasks"][task]["daimonos"]
                pct = (d["output_tokens"] - c["output_tokens"]) / c["output_tokens"] * 100 if c["output_tokens"] else 0
                print(f" {pct:>+14.1f}%", end="")
            else:
                print(f" {'N/A':>15}", end="")
        print()


if __name__ == "__main__":
    main()
