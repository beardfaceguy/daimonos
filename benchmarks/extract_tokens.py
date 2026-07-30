#!/usr/bin/env python3
"""Normalize per-task token usage across agent runtimes into one schema.

Each runtime reports usage differently; this collapses them to:
  input (fresh, non-cached) / cache_write / cache_read / output / total / cost
where total = input + cache_write + cache_read + output (matches Cursor's
admin-report "Total Tokens" definition, verified against a sample export).

Usage:
  extract_tokens.py <runtime> <rawFile> <tokenlog|-> <taskId> <taskName> \\
                    <modelSlug> <canon> <startedAt> <endedAt> <wallMs> \\
                    <exitCode> <outFile>

  runtime  = daimonos | claude | cursor | codex
  rawFile  = claude/cursor stream-json (.jsonl) or daimonos stdout (.txt)
  tokenlog = daimonos --debug-tokens delta (new lines only), or "-" otherwise
  exitCode = the CLI's exit code (feeds is_error)

Python port of the former extract-tokens.js (vikunja #1126); the output summary
JSON matches the JS it replaces (same fields, values, and semantics).
"""

import json
import sys
from datetime import datetime, timezone


def read_lines(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read().split("\n")
    except OSError:
        return []


def json_events(path):
    out = []
    for line in read_lines(path):
        stripped = line.strip()
        if not stripped:
            continue
        try:
            out.append(json.loads(stripped))
        except ValueError:
            continue
    return out


def count_tool_calls(events):
    """Count assistant tool_use blocks in a stream-json transcript (claude/cursor)."""
    n = 0
    for ev in events:
        if ev.get("type") != "assistant":
            continue
        content = (ev.get("message") or {}).get("content") or []
        if isinstance(content, list):
            for block in content:
                if block.get("type") == "tool_use":
                    n += 1
    return n


def parse_ms(ts):
    """Parse an ISO-8601 Z timestamp to epoch milliseconds, or None (mirrors
    JS Date.parse returning NaN -> treated as absent)."""
    if not ts:
        return None
    try:
        cleaned = ts.replace("Z", "+00:00")
        dt = datetime.fromisoformat(cleaned)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt.timestamp() * 1000.0
    except (ValueError, TypeError):
        return None


def main(argv):
    args = argv[1:]
    if len(args) < 12:
        sys.stderr.write(
            "usage: extract_tokens.py <runtime> <rawFile> <tokenlog> <taskId> "
            "<taskName> <modelSlug> <canon> <startedAt> <endedAt> <wallMs> "
            "<exitCode> <outFile>\n"
        )
        return 2
    (runtime, raw_file, tokenlog, task_id, task_name, model_slug, canon,
     started_at, ended_at, wall_ms_str, exit_code_str, out_file) = args[:12]

    try:
        wall_ms = int(wall_ms_str)
    except (ValueError, TypeError):
        wall_ms = 0
    try:
        exit_code = int(exit_code_str)
    except (ValueError, TypeError):
        exit_code = 0

    m = {"input": 0, "cache_write": 0, "cache_read": 0, "output": 0, "cost": 0}
    tool_calls = 0
    llm_calls = None
    is_error = True
    cost = None  # null = unknown (Cursor: comes from admin CSV later)

    if runtime == "claude":
        events = json_events(raw_file)
        result = next((e for e in events if e.get("type") == "result"), None)
        usage = (result.get("usage") or {}) if result else {}
        m["input"] = usage.get("input_tokens") or 0
        m["cache_write"] = usage.get("cache_creation_input_tokens") or 0
        m["cache_read"] = usage.get("cache_read_input_tokens") or 0
        m["output"] = usage.get("output_tokens") or 0
        cost = (result.get("total_cost_usd") or 0) if result else 0
        tool_calls = count_tool_calls(events)
        is_error = exit_code != 0 or (
            (result.get("is_error", True) if result else True)
        )
    elif runtime == "cursor":
        events = json_events(raw_file)
        result = next((e for e in events if e.get("type") == "result"), None)
        usage = (result.get("usage") or {}) if result else {}
        m["input"] = usage.get("inputTokens") or 0
        m["cache_write"] = usage.get("cacheWriteTokens") or 0
        m["cache_read"] = usage.get("cacheReadTokens") or 0
        m["output"] = usage.get("outputTokens") or 0
        cost = None  # cursor-agent does not emit cost; joined from admin CSV
        tool_calls = count_tool_calls(events)
        is_error = exit_code != 0 or (
            (result.get("is_error", True) if result else True)
        )
    elif runtime == "codex":
        # Codex CLI `exec --json` emits an event stream: the final response is an
        # `item.completed` event whose item.type is "agent_message" (item.text), and
        # usage is on the `turn.completed` event. Field names differ from cursor's
        # camelCase and claude's snake_case, so this needs its own branch.
        #   input_tokens             = total prompt tokens (includes the cached parts)
        #   cache_write_input_tokens = subset written to cache this turn
        #   cached_input_tokens      = subset read from cache
        #   output_tokens            = completion tokens
        # Map to the shared schema without double-counting: cache_write/cache_read
        # are the cached subsets, and `input` is the genuinely-fresh remainder, so
        # total = input + cache_write + cache_read + output stays consistent with
        # the other runtimes.
        events = json_events(raw_file)
        turn = next((e for e in events if e.get("type") == "turn.completed"), None)
        usage = (turn.get("usage") or {}) if turn else {}
        prompt_tokens = usage.get("input_tokens") or 0
        m["cache_write"] = usage.get("cache_write_input_tokens") or 0
        m["cache_read"] = usage.get("cached_input_tokens") or 0
        # Fresh (non-cached) input = prompt total minus the cached subsets, floored
        # at 0 in case a provider double-reports.
        m["input"] = max(0, prompt_tokens - m["cache_write"] - m["cache_read"])
        m["output"] = usage.get("output_tokens") or 0
        cost = None  # codex/OpenRouter exec stream carries no per-run cost here
        # Known limitation: codex emits tool activity as its own `item.completed`
        # item types (not the claude-style assistant `tool_use` block array that
        # count_tool_calls scans), so tool calls are reported as 0 here rather
        # than counted. Token/correctness metrics are unaffected; only the
        # informational tool_calls field is left at 0 for the codex runtime.
        tool_calls = 0
        is_error = exit_code != 0 or turn is None
    elif runtime == "daimonos":
        # Sum per-LLM-call lines from the --debug-tokens delta; skip non-call
        # event lines (e.g. compaction) which carry no input/output token fields.
        # The log is a global shared file, so additionally filter to this task's
        # time window (with skew slack) — a concurrent daimonos process appending
        # to the same log can't contaminate the sums.
        skew_ms = 2000
        win_start = parse_ms(started_at)
        win_end = parse_ms(ended_at)
        if win_start is not None:
            win_start -= skew_ms
        if win_end is not None:
            win_end += skew_ms
        has_window = win_start is not None and win_end is not None
        calls = 0
        saw_line = False
        for ev in json_events(tokenlog):
            if ev.get("event"):
                continue  # compaction / other structured events
            if not isinstance(ev.get("input"), (int, float)) and not isinstance(
                ev.get("output"), (int, float)
            ):
                continue
            if has_window and ev.get("ts"):
                t = parse_ms(ev.get("ts"))
                if t is not None and (t < win_start or t > win_end):
                    continue
            m["input"] += ev.get("input") or 0
            m["cache_write"] += ev.get("cache_write") or 0
            m["cache_read"] += ev.get("cache_read") or 0
            m["output"] += ev.get("output") or 0
            # cost_usd is logged as a fixed-decimal STRING (agent.rs), so coerce.
            try:
                m["cost"] += float(ev.get("cost_usd"))
            except (TypeError, ValueError):
                pass
            calls += 1
            saw_line = True
        cost = m["cost"]  # OpenRouter path often reports 0; tokens are primary
        tool_calls = None  # the token log records LLM calls, not tool invocations
        llm_calls = calls
        is_error = exit_code != 0 or not saw_line
    else:
        sys.stderr.write("unknown runtime: " + runtime + "\n")
        return 2

    total = m["input"] + m["cache_write"] + m["cache_read"] + m["output"]

    summary = {
        "task_id": task_id,
        "task_name": task_name,
        "runtime": runtime,
        "canon_model": canon,
        "model_slug": model_slug,
        "started_at": started_at,
        "ended_at": ended_at,
        "wall_ms": wall_ms,
        "input": m["input"],
        "cache_write": m["cache_write"],
        "cache_read": m["cache_read"],
        "output": m["output"],
        "total_tokens": total,
        "cost_usd": cost,
        "tool_calls": tool_calls,
        "llm_calls": llm_calls,
        "exit_code": exit_code,
        "is_error": is_error,
        "success": not is_error,  # upgraded to correctness-gated by check_task.py
    }

    with open(out_file, "w", encoding="utf-8") as handle:
        json.dump(summary, handle, indent=2)

    cost_str = "n/a (csv)" if cost is None else ("$" + format(float(cost), ".4f"))
    calls_str = ("llm-calls:" + str(llm_calls)) if tool_calls is None else ("tools:" + str(tool_calls))
    print(
        "       tokens: " + f"{total:,}"
        + " (in:" + f"{m['input']:,}"
        + " cw:" + f"{m['cache_write']:,}"
        + " cr:" + f"{m['cache_read']:,}"
        + " out:" + f"{m['output']:,}" + ")"
        + " | " + calls_str
        + " | cost:" + cost_str
        + " | wall:" + f"{wall_ms:,}" + "ms"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
