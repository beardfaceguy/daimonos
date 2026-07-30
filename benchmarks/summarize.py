#!/usr/bin/env python3
"""Print the end-of-run per-task summary table for a benchmark results dir.

Shared by the bench-*.sh runners (replaces the inline `node -e` summary blocks
they each carried; vikunja #1126). Reads every ``*.json`` task summary in the
given run dir and prints a fixed-width table + totals.

Usage: summarize.py <run-dir>

`cost_usd` may be null (some runtimes emit no per-run cost) — such rows show
"n/a" and are excluded from the cost total, which then prints "n/a" if no row
carried a cost.
"""

import glob
import json
import os
import sys


def load_rows(run_dir):
    rows = []
    for path in glob.glob(os.path.join(run_dir, "*.json")):
        try:
            with open(path, encoding="utf-8") as handle:
                rows.append(json.load(handle))
        except (ValueError, OSError) as exc:
            # Don't silently drop a malformed summary — that would under-report
            # results with no indication. Warn and skip so the table still prints.
            sys.stderr.write(f"warning: skipping unreadable summary {path}: {exc}\n")
            continue
    rows.sort(key=lambda r: str(r.get("task_id", "")))
    return rows


def main(argv):
    if len(argv) < 2:
        sys.stderr.write("usage: summarize.py <run-dir>\n")
        return 2
    run_dir = argv[1]
    rows = load_rows(run_dir)
    if not rows:
        print("(no task summaries)")
        return 0

    def pad(s, n):
        return str(s).ljust(n)

    def rpad(s, n):
        return str(s).rjust(n)

    print(pad("task", 22) + rpad("in", 9) + rpad("out", 8)
          + rpad("cost$", 9) + rpad("wall_s", 7) + "  correct")

    ti = to = tw = 0
    tc = 0.0
    any_cost = False
    correct = checked = 0
    for r in rows:
        i = r.get("input") or 0
        o = r.get("output") or 0
        w = (r.get("wall_ms") or 0) / 1000.0
        ti += i
        to += o
        tw += w
        c = r.get("cost_usd")
        if c is None:
            cost_str = "n/a"
        else:
            any_cost = True
            tc += float(c)
            cost_str = format(float(c), ".4f")
        if r.get("correct") is True:
            verdict = "OK"
            correct += 1
            checked += 1
        elif r.get("correct") is False:
            verdict = "INCORRECT"
            checked += 1
        else:
            verdict = "\u2014"
        chk = f"{r.get('checks_passed', '?')}/{r.get('checks_total', '?')}"
        print(pad(r.get("task_id", ""), 22) + rpad(i, 9) + rpad(o, 8)
              + rpad(cost_str, 9) + rpad(f"{w:.1f}", 7)
              + "  " + verdict + " (" + chk + ")")

    print("-" * 60)
    total_cost = format(tc, ".4f") if any_cost else "n/a"
    print(pad(f"TOTAL {len(rows)} tasks", 22) + rpad(ti, 9) + rpad(to, 8)
          + rpad(total_cost, 9) + rpad(f"{tw:.1f}", 7)
          + "  " + f"{correct}/{checked} correct")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
