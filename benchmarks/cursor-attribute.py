#!/usr/bin/env python3
"""Join Cursor admin usage-report rows to per-task summaries by time window.

cursor-agent emits token counts inline, but not dollar cost. The Cursor admin
"team usage events" CSV has real per-request cost. Each benchmark task run is
timestamped (started_at/ended_at, UTC) by run-runtime-benchmark.sh, so we
attribute CSV rows whose event Date falls in a task's window and write the
summed Cost back into that task's summary (cost_usd), plus a token cross-check.

Usage:
  cursor-attribute.py <cursor-run-dir> <team-usage-events.csv> [--buffer SECONDS]

The buffer (default 5s) widens each task window to absorb clock skew between the
local machine and Cursor's servers. Tasks run sequentially so windows don't
overlap. Rows already attributed to one task are not reused for another.
"""
import csv
import json
import os
import sys
from datetime import datetime, timedelta, timezone


def parse_iso(s):
    s = s.strip().replace("Z", "+00:00")
    dt = datetime.fromisoformat(s)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def to_int(s):
    try:
        return int(float(s))
    except (ValueError, TypeError):
        return 0


def main():
    # Single-pass arg parsing: --buffer consumes its value so a bare number
    # is never mistaken for a positional argument.
    args = []
    buffer_s = 5
    it = iter(sys.argv[1:])
    for a in it:
        if a == "--buffer":
            try:
                buffer_s = int(next(it))
            except (StopIteration, ValueError):
                print("--buffer requires an integer argument")
                sys.exit(1)
        else:
            args.append(a)
    if len(args) < 2:
        print(__doc__)
        sys.exit(1)
    run_dir, csv_path = args[0], args[1]

    # Load CSV rows with a parsed UTC datetime.
    rows = []
    with open(csv_path, newline="") as f:
        for r in csv.DictReader(f):
            try:
                r["_dt"] = parse_iso(r["Date"])
            except (ValueError, KeyError):
                continue
            rows.append(r)
    rows.sort(key=lambda r: r["_dt"])
    used = set()

    # Collect cursor task summaries with their raw windows first, sorted by
    # start time, so buffered windows can be clamped against neighbors — the
    # buffer must never extend into an adjacent task's actual run window.
    tasks = []
    for fn in sorted(os.listdir(run_dir)):
        if not fn.endswith(".json"):
            continue
        path = os.path.join(run_dir, fn)
        with open(path) as f:
            s = json.load(f)
        if s.get("runtime") != "cursor" or "started_at" not in s:
            continue
        try:
            raw_start = parse_iso(s["started_at"])
            raw_end = parse_iso(s["ended_at"])
        except (ValueError, KeyError):
            continue
        tasks.append({"path": path, "summary": s, "raw_start": raw_start,
                      "raw_end": raw_end})
    tasks.sort(key=lambda t: t["raw_start"])

    buf = timedelta(seconds=buffer_s)
    for i, t in enumerate(tasks):
        start = t["raw_start"] - buf
        end = t["raw_end"] + buf
        if i > 0:
            start = max(start, tasks[i - 1]["raw_end"])
        if i + 1 < len(tasks):
            end = min(end, tasks[i + 1]["raw_start"])
        t["start"], t["end"] = start, end

    updated = 0
    for t in tasks:
        path, s = t["path"], t["summary"]
        start, end = t["start"], t["end"]

        cost = 0.0
        csv_total = 0
        matched = 0
        for i, r in enumerate(rows):
            if i in used:
                continue
            if start <= r["_dt"] <= end:
                used.add(i)
                matched += 1
                try:
                    cost += float(r.get("Cost", "0") or 0)
                except ValueError:
                    pass
                csv_total += to_int(r.get("Total Tokens"))

        s["cursor_csv_rows"] = matched
        if matched == 0:
            # Report almost certainly hasn't populated this window yet; leave
            # cost_usd null (analyzer shows n/a) rather than a misleading $0.
            print(f"  {s['task_id']:22s} rows=0 (no CSV match yet; cost left null)")
            with open(path, "w") as f:
                json.dump(s, f, indent=2)
            updated += 1
            continue
        s["cost_usd"] = round(cost, 6)
        s["cursor_csv_total_tokens"] = csv_total
        # Sanity flag: inline token total vs CSV total (should be close).
        inline_total = s.get("total_tokens", 0) or 0
        if csv_total and inline_total:
            drift = abs(csv_total - inline_total) / max(csv_total, inline_total)
            s["cursor_csv_drift"] = round(drift, 3)
        with open(path, "w") as f:
            json.dump(s, f, indent=2)
        updated += 1
        print(f"  {s['task_id']:22s} rows={matched} cost=${cost:.4f} "
              f"csv_tokens={csv_total:,} inline={inline_total:,}")

    if updated == 0:
        print("No cursor task summaries with timestamps found in " + run_dir)
        sys.exit(1)
    print(f"Updated {updated} cursor task summar{'y' if updated == 1 else 'ies'}.")


if __name__ == "__main__":
    main()
