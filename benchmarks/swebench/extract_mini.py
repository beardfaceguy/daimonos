#!/usr/bin/env python3
"""Convert a mini-swe-agent trajectory into the shared per-instance summary
schema (same fields the daimonos/cursor runners emit), so all three harnesses
can be compared with one script.

Usage: extract_mini.py TRAJ_JSON INSTANCE_ID REPO MODEL OUT_JSON
"""
import json
import math
import sys


def token_count(value, field):
    value = value or 0
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"{field} must be a non-negative integer")
    return value


def main():
    traj_path, iid, repo, model, out_path = sys.argv[1:6]
    t = json.load(open(traj_path))
    tot_in = tot_out = cache_write = cache_read = calls = 0
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
            call_input = token_count(u.get("prompt_tokens"), "prompt_tokens")
            call_output = token_count(
                u.get("completion_tokens"), "completion_tokens"
            )
            prompt_details = u.get("prompt_tokens_details") or {}
            if not isinstance(prompt_details, dict):
                raise ValueError("prompt_tokens_details must be an object")
            call_cache_write = token_count(
                prompt_details.get("cache_write_tokens"), "cache_write_tokens"
            )
            call_cache_read = token_count(
                prompt_details.get("cached_tokens"), "cached_tokens"
            )
            if call_cache_write + call_cache_read > call_input:
                raise ValueError(
                    "cache token subsets exceed prompt_tokens "
                    f"on generation {calls}"
                )
            tot_in += call_input
            tot_out += call_output
            cache_write += call_cache_write
            cache_read += call_cache_read
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
    fresh_input = max(0, tot_in - cache_write - cache_read)
    summary = {
        "task_id": iid,
        "task_name": repo,
        "runtime": "mini-swe-agent",
        "canon_model": model,
        "model_slug": model,
        "wall_ms": wall_ms,
        "input": fresh_input,
        "cache_write": cache_write,
        "cache_read": cache_read,
        "output": tot_out,
        "total_tokens": tot_in + tot_out,
        "prompt_tokens": tot_in,
        "fresh_input_tokens": fresh_input,
        "mean_prompt_tokens_per_call": tot_in / calls if calls else None,
        "mean_cache_read_per_call": cache_read / calls if calls else None,
        "cache_hit_ratio": cache_read / tot_in if tot_in else None,
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
