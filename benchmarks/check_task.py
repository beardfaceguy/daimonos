#!/usr/bin/env python3
"""Per-task correctness gate (#929): evaluate a task's machine-checkable
``checks`` against the agent's final response and the workspace it left behind,
then stamp checks_passed / checks_total / correct into the task's summary json.
"The agent didn't crash" is not the same as "the agent did the task" — savings
measured on failed runs must not count.

Usage: check_task.py <task.json> <raw> <workspace-dir> <summary.json> [format]

Check shapes (in the task json's "checks" array):
  {"type": "response", "all": ["pat", ...]}            every regex matches (case-insensitive)
  {"type": "response", "any": ["pat", ...], "min": 2}  at least min regexes match
  {"type": "workspace", "command": "sh command"}       exit 0 in the workspace = pass

Python port of the former check-task.js (vikunja #1126); behavior and output
JSON are byte-for-byte identical to the JS it replaces.
"""

import json
import re
import subprocess
import sys


# format: "stream-json" (default; claude/cursor emit a JSON event stream) or
# "text" (daimonos agent writes plain assistant text to stdout, not events).
def final_text(raw_file, raw_format):
    """The final response text lives in the stream's ``result`` event; fall back
    to concatenated assistant text blocks for streams that lack one."""
    try:
        with open(raw_file, encoding="utf-8") as handle:
            raw = handle.read()
    except OSError:
        return ""
    # daimonos agent: the whole stdout IS the response text.
    if raw_format == "text":
        return raw
    assistant_text = ""
    for line in raw.split("\n"):
        if not line.strip():
            continue
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "result" and isinstance(ev.get("result"), str):
            return ev["result"]
        if ev.get("type") == "assistant":
            content = (ev.get("message") or {}).get("content") or []
            for block in content:
                if block.get("type") == "text" and block.get("text"):
                    assistant_text += block["text"] + "\n"
    return assistant_text


def run_check(check, text, workspace):
    if check.get("type") == "response":
        # Patterns are authored in this repo's task JSONs, not user input.
        def matches(pat):
            return re.search(pat, text, re.IGNORECASE) is not None

        if check.get("all") is not None:
            return all(matches(p) for p in check["all"])
        if check.get("any") is not None:
            hits = sum(1 for p in check["any"] if matches(p))
            return hits >= check.get("min", 1)
        return False
    if check.get("type") == "workspace":
        # Executing a configured shell command IS the feature here: workspace
        # checks are commands like `grep -q display_names src/config.rs` that
        # assert filesystem ground truth. `check.command` is not user input —
        # it comes from the version-controlled task JSONs in this repo and the
        # harness is developer-invoked, not a service.
        try:
            subprocess.run(
                check["command"],
                cwd=workspace,
                # Executing the task's configured shell command is the feature;
                # it comes from version-controlled task JSONs, not user input.
                shell=True,  # nosemgrep: python.lang.security.audit.subprocess-shell-true.subprocess-shell-true
                executable="/bin/sh",
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=120,
                check=True,
            )
            return True
        except (subprocess.SubprocessError, OSError):
            return False
    sys.stderr.write(f"unknown check type: {check.get('type')}\n")
    return False


def main(argv):
    args = argv[1:]
    if len(args) < 4:
        sys.stderr.write(
            "usage: check_task.py <task.json> <raw> <workspace> <summary.json> [format]\n"
        )
        return 2
    task_file, raw_file, workspace, summary_file = args[0], args[1], args[2], args[3]
    raw_format = args[4] if len(args) > 4 else "stream-json"

    with open(task_file, encoding="utf-8") as handle:
        task = json.load(handle)
    checks = task.get("checks") or []

    text = final_text(raw_file, raw_format)
    passed = sum(1 for check in checks if run_check(check, text, workspace))

    with open(summary_file, encoding="utf-8") as handle:
        summary = json.load(handle)
    summary["checks_total"] = len(checks)
    summary["checks_passed"] = passed
    summary["correct"] = None if len(checks) == 0 else (passed == len(checks))
    with open(summary_file, "w", encoding="utf-8") as handle:
        json.dump(summary, handle, indent=2)

    verdict = (
        "no checks"
        if summary["correct"] is None
        else ("correct" if summary["correct"] else "INCORRECT")
    )
    print(f"       checks: {passed}/{len(checks)} ({verdict})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
