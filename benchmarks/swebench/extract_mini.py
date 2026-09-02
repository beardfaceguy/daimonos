#!/usr/bin/env python3
"""Convert a mini-swe-agent trajectory into the shared per-instance summary
schema (same fields the daimonos/cursor runners emit), so all three harnesses
can be compared with one script.

Usage: extract_mini.py TRAJ_JSON INSTANCE_ID REPO MODEL OUT_JSON
"""
import json
import sys


def main():
    traj_path, iid, repo, model, out_path = sys.argv[1:6]
    t = json.load(open(traj_path))
    tot_in = tot_out = calls = 0
    timestamps = []
    for m in t.get("messages", []):
        extra = m.get("extra") or {}
        u = (extra.get("response") or {}).get("usage")
        if u:
            calls += 1
            tot_in += u.get("prompt_tokens", 0) or 0
            tot_out += u.get("completion_tokens", 0) or 0
        if "timestamp" in extra:
            timestamps.append(extra["timestamp"])
    wall_ms = int((max(timestamps) - min(timestamps)) * 1000) if len(timestamps) > 1 else 0
    exit_status = (t.get("info") or {}).get("exit_status")
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
        "cost_usd": None,
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
