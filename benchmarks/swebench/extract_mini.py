#!/usr/bin/env python3
"""Convert a mini-swe-agent trajectory into the shared per-instance summary
schema (same fields the daimonos/cursor runners emit), so all three harnesses
can be compared with one script.

Usage: extract_mini.py TRAJ_JSON INSTANCE_ID REPO MODEL OUT_JSON
"""
import json
import math
import sys


def main():
    traj_path, iid, repo, model, out_path = sys.argv[1:6]
    t = json.load(open(traj_path))
    tot_in = tot_out = calls = 0
    costs = []
    cost_complete = True
    timestamps = []
    for m in t.get("messages", []):
        extra = m.get("extra") or {}
        if "response" in extra:
            calls += 1
            response = extra.get("response")
            usage = response.get("usage") if isinstance(response, dict) else None
            if not isinstance(usage, dict) or not usage:
                cost_complete = False
                usage = {}

            u = usage
            tot_in += u.get("prompt_tokens", 0) or 0
            tot_out += u.get("completion_tokens", 0) or 0
            cost = u.get("cost")
            if (
                isinstance(cost, (int, float))
                and not isinstance(cost, bool)
                and cost >= 0
            ):
                costs.append(cost)
            else:
                cost_complete = False
        if "timestamp" in extra:
            timestamps.append(extra["timestamp"])
    wall_ms = int((max(timestamps) - min(timestamps)) * 1000) if len(timestamps) > 1 else 0
    exit_status = (t.get("info") or {}).get("exit_status")
    provider_cost = math.fsum(costs) if calls and cost_complete else None
    model_stats_cost = (
        (t.get("info") or {}).get("model_stats") or {}
    ).get("instance_cost")
    if not (
        isinstance(model_stats_cost, (int, float))
        and not isinstance(model_stats_cost, bool)
        and model_stats_cost >= 0
    ):
        model_stats_cost = None
    cost_matches_model_stats = (
        math.isclose(provider_cost, model_stats_cost, rel_tol=1e-9, abs_tol=1e-12)
        if provider_cost is not None and model_stats_cost is not None
        else None
    )
    summary = {
        "task_id": iid,
        "task_name": repo,
        "runtime": "mini-swe-agent",
        "canon_model": model,
        "model_slug": model,
        "wall_ms": wall_ms,
        "input": tot_in,
        "output": tot_out,
        "total_tokens": tot_in + tot_out,
        "prompt_tokens": tot_in,
        "llm_calls": calls,
        # Same accounting source as Daimonos: OpenRouter's per-generation
        # usage.cost, not mini-swe-agent/LiteLLM's model-price estimate.
        "cost_usd": provider_cost,
        "cost_source": "openrouter_usage" if provider_cost is not None else None,
        "model_stats_cost_usd": model_stats_cost,
        "cost_matches_model_stats": cost_matches_model_stats,
        "exit_code": 0 if exit_status == "Submitted" else 1,
        "is_error": exit_status != "Submitted",
        "success": exit_status == "Submitted",
        "swebench_instance_id": iid,
    }
    with open(out_path, "w") as f:
        json.dump(summary, f, indent=2)
    print(f"       tokens: {tot_in + tot_out:,} (in:{tot_in:,} out:{tot_out:,}) | "
          f"llm-calls:{calls} | exit:{exit_status}")


if __name__ == "__main__":
    main()
