#!/usr/bin/env python3
"""Append a new stage to benchmarks/optimization-lineage.json from finished run dirs.

Reads the per-task summary JSONs a bench run wrote under results/<run-dir>/,
aggregates them into the same metric keys the existing full_suite stages use
(mean_total_tokens / mean_cost_usd / mean_wall_ms / correct_task_runs, plus a
few optional cache/output means when present), stamps the binary sha256 and git
commit, and appends the stage to the lineage file.

To stay comparable with its parent, an appended stage inherits the parent's
scope_fingerprint, provider, model, thinking and task_ids unless you override
them. That keeps an A/B pair (e.g. pre-opt F-parent vs post-opt new stage) on
one axis.

Usage:
  lineage_append.py --id F3 --parent F1 \
      --feature "End of #122-#126 optimization round" \
      --binary ../bench/bin/daimonos-a31e470 \
      RESULTS_DIR [RESULTS_DIR ...]

  RESULTS_DIR   One or more finished run dirs under results/ (metrics are the
                mean across them; correct_task_runs is the total across them).

Key options:
  --git-commit SHA   Defaults to the trailing hex of the binary filename
                     (daimonos-<sha>), else `git rev-parse HEAD`.
  --scope-fingerprint / --provider / --model / --thinking / --benchmark-kind
                     Override; otherwise inherited from the --parent stage
                     (benchmark_kind defaults to full_suite when no parent).
  --report-path PATH        Optional report_path for the stage.
  --artifact PATH:KIND      Repeatable; adds {path, sha256, kind} artifact rows.
  --lineage PATH            Lineage file (default: alongside this script).
  --dry-run                 Print the stage JSON, do not write.
"""

import argparse
import glob
import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_LINEAGE = os.path.join(HERE, "optimization-lineage.json")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_task_summaries(run_dir):
    """Per-task summary dicts in one run dir (skips *aggregate* files)."""
    out = []
    for path in sorted(glob.glob(os.path.join(run_dir, "*.json"))):
        if "aggregate" in os.path.basename(path):
            continue
        try:
            with open(path) as f:
                s = json.load(f)
        except (ValueError, OSError):
            continue
        if isinstance(s, dict) and "task_id" in s:
            out.append(s)
    return out


def num(v):
    try:
        return float(v) if v is not None else None
    except (TypeError, ValueError):
        return None


def aggregate(run_dirs):
    """Return (metrics_dict, task_ids, model_slug, n_runs)."""
    per_run_tokens, per_run_cost, per_run_wall = [], [], []
    per_run_output, per_run_cache_read, per_run_cache_write, per_run_prompt = [], [], [], []
    per_run_llm_calls = []
    correct_task_runs = 0
    task_ids, models = set(), {}
    used = 0

    for rd in run_dirs:
        summaries = load_task_summaries(rd)
        if not summaries:
            print(f"warning: no task summaries in {rd}", file=sys.stderr)
            continue
        used += 1
        tok = cost = wall = out = cr = cw = prm = llm = 0.0
        have_cost = have_cache = have_prompt = have_llm = False
        for s in summaries:
            task_ids.add(s["task_id"])
            m = s.get("model_slug") or s.get("model")
            if m:
                models[m] = models.get(m, 0) + 1
            if s.get("correct") is True:
                correct_task_runs += 1
            tok += num(s.get("total_tokens")) or 0
            wall += num(s.get("wall_ms")) or 0
            out += num(s.get("output")) or 0
            c = num(s.get("cost_usd"))
            if c is not None:
                cost += c
                have_cost = True
            crv, cwv = num(s.get("cache_read")), num(s.get("cache_write"))
            if crv is not None or cwv is not None:
                cr += crv or 0
                cw += cwv or 0
                have_cache = True
            pv = num(s.get("input"))
            if pv is not None:
                prm += pv
                have_prompt = True
            lv = num(s.get("llm_calls"))
            if lv is not None:
                llm += lv
                have_llm = True
        per_run_tokens.append(tok)
        per_run_wall.append(wall)
        per_run_output.append(out)
        if have_cost:
            per_run_cost.append(cost)
        if have_cache:
            per_run_cache_read.append(cr)
            per_run_cache_write.append(cw)
        if have_prompt:
            per_run_prompt.append(prm)
        if have_llm:
            per_run_llm_calls.append(llm)

    def mean(xs):
        return sum(xs) / len(xs) if xs else None

    metrics = {}
    if per_run_tokens:
        metrics["mean_total_tokens"] = [mean(per_run_tokens), "tokens"]
    if per_run_prompt:
        metrics["mean_fresh_input_tokens"] = [mean(per_run_prompt), "tokens"]
    if per_run_cache_read:
        metrics["mean_cache_read_tokens"] = [mean(per_run_cache_read), "tokens"]
    if per_run_cache_write:
        metrics["mean_cache_write_tokens"] = [mean(per_run_cache_write), "tokens"]
    if per_run_output:
        metrics["mean_output_tokens"] = [mean(per_run_output), "tokens"]
    if per_run_llm_calls:
        metrics["mean_llm_calls"] = [mean(per_run_llm_calls), "count"]
    if per_run_cost:
        metrics["mean_cost_usd"] = [mean(per_run_cost), "usd"]
    if per_run_wall:
        metrics["mean_wall_ms"] = [mean(per_run_wall), "milliseconds"]
    metrics["correct_task_runs"] = [correct_task_runs, "count"]

    model_slug = max(models, key=models.get) if models else None
    return metrics, sorted(task_ids), model_slug, used


def infer_git_commit(binary_path):
    base = os.path.basename(binary_path)
    m = re.search(r"([0-9a-f]{7,40})", base)
    if m:
        try:
            full = subprocess.check_output(
                ["git", "-C", HERE, "rev-parse", m.group(1)],
                stderr=subprocess.DEVNULL,
            ).decode().strip()
            return full
        except subprocess.CalledProcessError:
            return m.group(1)
    try:
        return subprocess.check_output(
            ["git", "-C", HERE, "rev-parse", "HEAD"], stderr=subprocess.DEVNULL
        ).decode().strip()
    except subprocess.CalledProcessError:
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("run_dirs", nargs="+", help="Finished results/<run-dir> paths")
    ap.add_argument("--id", required=True, help="New stage id (e.g. F3)")
    ap.add_argument("--parent", required=True, help="Parent stage id, or 'none' for a root")
    ap.add_argument("--feature", required=True, help="Human-readable feature label")
    ap.add_argument("--binary", required=True, help="Path to the benchmarked binary")
    ap.add_argument("--git-commit", default=None)
    ap.add_argument("--scope-fingerprint", default=None)
    ap.add_argument("--benchmark-kind", default=None)
    ap.add_argument("--provider", default=None)
    ap.add_argument("--model", default=None)
    ap.add_argument("--thinking", default=None)
    ap.add_argument("--report-path", default=None)
    ap.add_argument("--artifact", action="append", default=[], help="PATH:KIND (repeatable)")
    ap.add_argument("--lineage", default=DEFAULT_LINEAGE)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    if not os.path.isfile(args.binary):
        ap.error(f"binary not found: {args.binary}")
    for rd in args.run_dirs:
        if not os.path.isdir(rd):
            ap.error(f"run dir not found: {rd}")

    with open(args.lineage) as f:
        lineage = json.load(f)
    stages = lineage.setdefault("stages", [])
    by_id = {s["id"]: s for s in stages}

    if args.id in by_id:
        ap.error(f"stage id '{args.id}' already exists in {args.lineage}")
    parent = None if args.parent.lower() == "none" else args.parent
    if parent is not None and parent not in by_id:
        ap.error(f"parent stage '{parent}' not found (have: {', '.join(by_id) or 'none'})")
    pstage = by_id.get(parent, {})

    metrics, task_ids, model_slug, n_runs = aggregate(args.run_dirs)
    if n_runs == 0:
        ap.error("no usable run dirs (no per-task summaries found)")

    repo_root = os.path.dirname(HERE)

    def rel(p):
        return os.path.relpath(os.path.abspath(p), repo_root)

    def resolve_repo_rel(p):
        # Accept EITHER a cwd-relative OR a repo-root-relative path and always
        # store it repo-root-relative (matching existing lineage entries). Picks
        # whichever interpretation lands on an existing file or existing parent
        # dir, so `benchmarks/results/x.md` from the benchmarks/ cwd is not
        # doubled into `benchmarks/benchmarks/results/x.md`.
        cwd_cand = os.path.abspath(p)
        repo_cand = os.path.abspath(os.path.join(repo_root, p))
        for c in (cwd_cand, repo_cand):
            if os.path.exists(c) or os.path.isdir(os.path.dirname(c)):
                return os.path.relpath(c, repo_root)
        return os.path.relpath(cwd_cand, repo_root)

    artifacts = []
    for spec in args.artifact:
        if ":" not in spec:
            ap.error(f"--artifact must be PATH:KIND, got '{spec}'")
        apath, kind = spec.rsplit(":", 1)
        row = {"path": resolve_repo_rel(apath), "kind": kind}
        if os.path.isfile(apath):
            row["sha256"] = sha256_file(apath)
        artifacts.append(row)

    stage = {
        "id": args.id,
        "parent": parent,
        "feature": args.feature,
        "scope_fingerprint": args.scope_fingerprint or pstage.get("scope_fingerprint"),
        "benchmark_kind": args.benchmark_kind or pstage.get("benchmark_kind") or "full_suite",
        "provider": args.provider if args.provider is not None else pstage.get("provider"),
        "model": args.model or model_slug or pstage.get("model"),
        "thinking": args.thinking if args.thinking is not None else pstage.get("thinking"),
        "task_ids": task_ids or pstage.get("task_ids", []),
        "binary_sha256": sha256_file(args.binary),
        "git_commit": args.git_commit or infer_git_commit(args.binary),
        "created_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "run_dirs": [os.path.basename(rd.rstrip("/")) for rd in args.run_dirs],
        "metrics": metrics,
    }
    if args.report_path:
        stage["report_path"] = resolve_repo_rel(args.report_path)
    if artifacts:
        stage["artifacts"] = artifacts

    print(json.dumps(stage, indent=2))
    if args.dry_run:
        print("\n[dry-run] not written", file=sys.stderr)
        return

    backup = args.lineage + ".bak"
    with open(backup, "w") as f:
        json.dump(lineage, f, indent=2)
    stages.append(stage)
    with open(args.lineage, "w") as f:
        json.dump(lineage, f, indent=2)
        f.write("\n")
    print(f"\nappended stage '{args.id}' -> {args.lineage} (backup: {backup})", file=sys.stderr)


if __name__ == "__main__":
    main()
