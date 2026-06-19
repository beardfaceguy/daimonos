#!/usr/bin/env python3
"""Analyze and compare benchmark results across cursor and daimonos runs."""

import json
import sys
from pathlib import Path


def load_run(run_dir: Path) -> dict:
    """Load all task results from a run directory."""
    results = {}
    for f in sorted(run_dir.glob("*.json")):
        if f.name.endswith(".raw.jsonl"):
            continue
        data = json.loads(f.read_text())
        results[data["task_id"]] = data
    return results


def find_runs(results_dir: Path, mode: str, tag: str = "") -> list[Path]:
    """Find run directories for a given mode and optional tag."""
    aliases = [mode]
    if mode == "baseline":
        aliases.append("cursor")
    runs = []
    for d in sorted(results_dir.iterdir()):
        if not d.is_dir():
            continue
        name = d.name
        parts = name.split("-", 2)  # date-time-rest
        if len(parts) < 3:
            continue
        rest = parts[2]
        for alias in aliases:
            if tag:
                if rest == f"{alias}-{tag}":
                    runs.append(d)
            else:
                if rest == alias:
                    runs.append(d)
    return runs


def latest_run(run_dirs: list[Path]) -> Path | None:
    return run_dirs[-1] if run_dirs else None


def print_comparison(baseline_results: dict, daimonos_results: dict):
    all_tasks = sorted(set(list(baseline_results.keys()) + list(daimonos_results.keys())))

    print("\n" + "=" * 130)
    print(f"{'Task':<35} {'Mode':<10} {'Out Tok':>8} {'Cache R':>9} {'Cache W':>9} {'Tools':>6} {'MCP':>5} {'Cost':>8} {'Wall ms':>9} {'OK':>4}")
    print("=" * 130)

    for task_id in all_tasks:
        b = baseline_results.get(task_id)
        d = daimonos_results.get(task_id)

        if b:
            cost_str = f"${b.get('cost_usd', 0):.4f}"
            print(f"{b['task_name'][:35]:<35} {'baseline':<10} {b['output_tokens']:>8,} {b['cache_read_tokens']:>9,} {b['cache_write_tokens']:>9,} {b['tool_calls']:>6} {'':>5} {cost_str:>8} {b['wall_ms']:>9,} {'Y' if b['success'] else 'N':>4}")

        if d:
            mcp = d.get('mcp_tool_calls', 0)
            cost_str = f"${d.get('cost_usd', 0):.4f}"
            print(f"{d['task_name'][:35]:<35} {'daimonos':<10} {d['output_tokens']:>8,} {d['cache_read_tokens']:>9,} {d['cache_write_tokens']:>9,} {d['tool_calls']:>6} {mcp:>5} {cost_str:>8} {d['wall_ms']:>9,} {'Y' if d['success'] else 'N':>4}")

        if b and d:
            out_diff = d["output_tokens"] - b["output_tokens"]
            cache_diff = d["cache_read_tokens"] - b["cache_read_tokens"]
            tool_diff = d["tool_calls"] - b["tool_calls"]
            wall_diff = d["wall_ms"] - b["wall_ms"]
            out_pct = (out_diff / b["output_tokens"] * 100) if b["output_tokens"] > 0 else 0
            cost_diff = d.get("cost_usd", 0) - b.get("cost_usd", 0)
            cost_str = f"${cost_diff:+.4f}"
            print(f"{'  delta':<35} {'':10} {out_diff:>+8,} {cache_diff:>+9,} {'':>9} {tool_diff:>+6} {'':>5} {cost_str:>8} {wall_diff:>+9,} {f'{out_pct:+.0f}%':>4}")

        print("-" * 130)

    print("\n" + "=" * 80)
    print("AGGREGATE (comparable tasks only)")
    print("=" * 80)

    shared_tasks = sorted(set(baseline_results.keys()) & set(daimonos_results.keys()))
    if not shared_tasks:
        print("No comparable tasks found.")
        return

    b_out = sum(baseline_results[t]["output_tokens"] for t in shared_tasks)
    d_out = sum(daimonos_results[t]["output_tokens"] for t in shared_tasks)
    b_cache = sum(baseline_results[t]["cache_read_tokens"] for t in shared_tasks)
    d_cache = sum(daimonos_results[t]["cache_read_tokens"] for t in shared_tasks)
    b_tools = sum(baseline_results[t]["tool_calls"] for t in shared_tasks)
    d_tools = sum(daimonos_results[t]["tool_calls"] for t in shared_tasks)
    d_mcp = sum(daimonos_results[t].get("mcp_tool_calls", 0) for t in shared_tasks)
    b_wall = sum(baseline_results[t]["wall_ms"] for t in shared_tasks)
    d_wall = sum(daimonos_results[t]["wall_ms"] for t in shared_tasks)
    b_cost = sum(baseline_results[t].get("cost_usd", 0) for t in shared_tasks)
    d_cost = sum(daimonos_results[t].get("cost_usd", 0) for t in shared_tasks)

    print(f"\n{'Metric':<25} {'Baseline':>12} {'Daimonos':>12} {'Delta':>12} {'Change':>10}")
    print("-" * 71)
    print(f"{'Output tokens':<25} {b_out:>12,} {d_out:>12,} {d_out - b_out:>+12,} {(d_out - b_out) / b_out * 100 if b_out else 0:>+9.1f}%")
    print(f"{'Cache read tokens':<25} {b_cache:>12,} {d_cache:>12,} {d_cache - b_cache:>+12,} {(d_cache - b_cache) / b_cache * 100 if b_cache else 0:>+9.1f}%")
    print(f"{'Tool calls':<25} {b_tools:>12,} {d_tools:>12,} {d_tools - b_tools:>+12,} {(d_tools - b_tools) / b_tools * 100 if b_tools else 0:>+9.1f}%")
    print(f"{'  of which MCP':<25} {'':>12} {d_mcp:>12,}")
    print(f"{'Actual cost (USD)':<25} {f'${b_cost:.4f}':>12} {f'${d_cost:.4f}':>12} {f'${d_cost - b_cost:+.4f}':>12} {(d_cost - b_cost) / b_cost * 100 if b_cost else 0:>+9.1f}%")
    print(f"{'Wall time (ms)':<25} {b_wall:>12,} {d_wall:>12,} {d_wall - b_wall:>+12,} {(d_wall - b_wall) / b_wall * 100 if b_wall else 0:>+9.1f}%")

    print(f"\nComparable tasks: {len(shared_tasks)}")
    if d_mcp > 0:
        print(f"MCP tool adoption: {d_mcp}/{d_tools} tool calls ({d_mcp/d_tools*100:.0f}%) were daimonos MCP tools")

    if d_cost < b_cost and b_cost > 0:
        print(f"Daimonos saves ${b_cost - d_cost:.4f} ({(b_cost - d_cost) / b_cost * 100:.1f}% cheaper)")
    elif b_cost > 0:
        print(f"Daimonos costs ${d_cost - b_cost:.4f} more ({(d_cost - b_cost) / b_cost * 100:.1f}% more)")

    print(f"\n{'Task':<35} {'Out Tok Delta':>14} {'Cost Delta':>12} {'MCP/Total':>10}")
    print("-" * 75)
    for t in shared_tasks:
        b = baseline_results[t]
        d = daimonos_results[t]
        out_d = d["output_tokens"] - b["output_tokens"]
        out_pct = (out_d / b["output_tokens"] * 100) if b["output_tokens"] else 0
        cost_d = d.get("cost_usd", 0) - b.get("cost_usd", 0)
        mcp_ratio = f"{d.get('mcp_tool_calls',0)}/{d['tool_calls']}"
        print(f"{b['task_name'][:35]:<35} {out_d:>+8,} ({out_pct:>+5.1f}%) {f'${cost_d:+.4f}':>12} {mcp_ratio:>10}")


def main():
    if len(sys.argv) < 2:
        print("Usage: python3 analyze-results.py <results-dir> [baseline-run] [daimonos-run]")
        print("\nIf run names are omitted, uses the most recent run of each mode.")
        sys.exit(1)

    results_dir = Path(sys.argv[1])
    tag = ""

    if len(sys.argv) >= 4:
        baseline_dir = results_dir / sys.argv[2]
        daimonos_dir = results_dir / sys.argv[3]
    elif len(sys.argv) >= 3:
        tag = sys.argv[2]
        baseline_dir = latest_run(find_runs(results_dir, "baseline", tag))
        daimonos_dir = latest_run(find_runs(results_dir, "daimonos", tag))
    else:
        baseline_dir = latest_run(find_runs(results_dir, "baseline"))
        daimonos_dir = latest_run(find_runs(results_dir, "daimonos"))

    baseline_results = load_run(baseline_dir) if baseline_dir and baseline_dir.exists() else {}
    daimonos_results = load_run(daimonos_dir) if daimonos_dir and daimonos_dir.exists() else {}

    if not baseline_results and not daimonos_results:
        print("No results found. Run benchmarks first:")
        print("  ./run-benchmark.sh baseline")
        print("  ./run-benchmark.sh daimonos")
        sys.exit(1)

    if baseline_dir and baseline_results:
        print(f"Baseline run: {baseline_dir.name}")
    if daimonos_dir and daimonos_results:
        print(f"Daimonos run: {daimonos_dir.name}")

    print_comparison(baseline_results, daimonos_results)


if __name__ == "__main__":
    main()
