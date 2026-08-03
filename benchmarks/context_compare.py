#!/usr/bin/env python3
"""Compare correctness-gated native-agent context composition between two arms."""

import argparse
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path


def load(paths):
    summaries = []
    for directory in paths:
        for path in sorted(Path(directory).glob("*.json")):
            try:
                summary = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if (
                summary.get("task_id")
                and not summary.get("is_error")
                and summary.get("correct") is not False
            ):
                summaries.append(summary)
    return summaries


def aggregate(summaries):
    calls = sum(summary.get("llm_calls") or 0 for summary in summaries)
    prompt_tokens = sum(summary.get("prompt_tokens") or 0 for summary in summaries)
    fresh_input = sum(
        summary.get("fresh_input_tokens", summary.get("input")) or 0
        for summary in summaries
    )
    cache_write = sum(summary.get("cache_write") or 0 for summary in summaries)
    cache_read = sum(summary.get("cache_read") or 0 for summary in summaries)
    output = sum(summary.get("output") or 0 for summary in summaries)
    wall_ms = sum(summary.get("wall_ms") or 0 for summary in summaries)
    costs = [summary.get("cost_usd") for summary in summaries]
    cost_usd = (
        sum(float(cost) for cost in costs)
        if costs and all(cost is not None for cost in costs)
        else None
    )
    context_calls = sum(summary.get("calls_with_context") or 0 for summary in summaries)
    components = defaultdict(float)
    for summary in summaries:
        for name, value in (
            summary.get("context_component_tokens_est_total") or {}
        ).items():
            components[name] += value
    ranked = sorted(components.items(), key=lambda item: (-item[1], item[0]))
    return {
        "task_runs": len(summaries),
        "calls": calls,
        "prompt_tokens": prompt_tokens,
        "fresh_input_tokens": fresh_input,
        "cache_write_tokens": cache_write,
        "cache_read_tokens": cache_read,
        "output_tokens": output,
        "cost_usd": cost_usd,
        "wall_ms": wall_ms,
        "mean_prompt_tokens_per_call": prompt_tokens / calls if calls else None,
        "mean_cache_read_per_call": cache_read / calls if calls else None,
        "context_coverage_pct": context_calls / calls * 100.0 if calls else None,
        "ranked_components": ranked,
        "component_tokens_est_total": dict(sorted(components.items())),
    }


def compare(baseline, candidate):
    baseline_mean = baseline["mean_prompt_tokens_per_call"] or 0
    candidate_mean = candidate["mean_prompt_tokens_per_call"] or 0
    baseline_calls = baseline["calls"]
    candidate_calls = candidate["calls"]
    total_delta = candidate["prompt_tokens"] - baseline["prompt_tokens"]
    call_effect = (
        (candidate_calls - baseline_calls)
        * (baseline_mean + candidate_mean)
        / 2.0
    )
    context_effect = (
        (candidate_mean - baseline_mean)
        * (baseline_calls + candidate_calls)
        / 2.0
    )
    component_names = set(baseline["component_tokens_est_total"]) | set(
        candidate["component_tokens_est_total"]
    )
    component_delta = {
        name: candidate["component_tokens_est_total"].get(name, 0)
        - baseline["component_tokens_est_total"].get(name, 0)
        for name in sorted(component_names)
    }
    cost_delta_pct = (
        (candidate["cost_usd"] / baseline["cost_usd"] - 1.0) * 100.0
        if baseline["cost_usd"] and candidate["cost_usd"] is not None
        else None
    )
    return {
        "total_prompt_token_delta": total_delta,
        "call_count_effect": call_effect,
        "mean_context_effect": context_effect,
        "reconstructed_delta": call_effect + context_effect,
        "cost_delta_pct": cost_delta_pct,
        "wall_delta_pct": (
            (candidate["wall_ms"] / baseline["wall_ms"] - 1.0) * 100.0
            if baseline["wall_ms"]
            else None
        ),
        "component_token_delta": component_delta,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", nargs="+", required=True)
    parser.add_argument("--candidate", nargs="+", required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)

    baseline_summaries = load(args.baseline)
    candidate_summaries = load(args.candidate)
    if not baseline_summaries or not candidate_summaries:
        parser.error("both arms need at least one correctness-gated task summary")
    baseline_tasks = Counter(summary["task_id"] for summary in baseline_summaries)
    candidate_tasks = Counter(summary["task_id"] for summary in candidate_summaries)
    if baseline_tasks != candidate_tasks:
        parser.error(
            "both arms need matching correctness-gated task runs; "
            f"baseline={dict(baseline_tasks)}, candidate={dict(candidate_tasks)}"
        )

    baseline = aggregate(baseline_summaries)
    candidate = aggregate(candidate_summaries)
    if not baseline["calls"] or not candidate["calls"]:
        parser.error("both arms need at least one LLM call")
    result = {
        "baseline": baseline,
        "candidate": candidate,
        "decomposition": compare(baseline, candidate),
    }
    if args.json:
        json.dump(result, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0

    decomposition = result["decomposition"]
    print("Context comparison")
    print(
        f"  baseline:  {baseline['calls']} calls, "
        f"{baseline['mean_prompt_tokens_per_call']:.1f} prompt tok/call"
    )
    print(
        f"  candidate: {candidate['calls']} calls, "
        f"{candidate['mean_prompt_tokens_per_call']:.1f} prompt tok/call"
    )
    print(f"  total prompt-token delta: {decomposition['total_prompt_token_delta']:+.0f}")
    print(f"  call-count effect:        {decomposition['call_count_effect']:+.1f}")
    print(f"  mean-context effect:      {decomposition['mean_context_effect']:+.1f}")
    if decomposition["cost_delta_pct"] is not None:
        print(f"  cost delta:               {decomposition['cost_delta_pct']:+.1f}%")
    if decomposition["wall_delta_pct"] is not None:
        print(f"  wall-time delta:          {decomposition['wall_delta_pct']:+.1f}%")
    print("  candidate component exposure:")
    for name, tokens in candidate["ranked_components"]:
        print(f"    {name:32s} {tokens:12.0f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
