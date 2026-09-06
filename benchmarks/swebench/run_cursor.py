#!/usr/bin/env python3
"""SWE-bench runner for cursor-agent (external-agent comparison).

Counterpart to run_agent.py: same instances, same checkout/diff/prediction
flow, but the agent is `cursor-agent` in headless mode. Token accounting uses
../extract_tokens.py (cursor branch: usage is inline in the stream-json result
event). As with ../bench-cursor.sh, cursor bills Cursor's backend — token and
correctness parity only, no cost parity.

Usage:
  .venv/bin/python run_cursor.py --model MODEL [--instance-ids IDS]
                                 [--filter PREFIX] [--tag TAG]
                                 [--timeout SECS] [--keep]
"""
import argparse
import datetime as dt
import json
import os
import pathlib
import shutil
import subprocess
import sys

import run_agent  # reuse checkout, collect_patch, INSTANCES, dirs

HERE = pathlib.Path(__file__).parent
BENCH_ROOT = HERE.parent
EXTRACT = BENCH_ROOT / "extract_tokens.py"
CURSOR_BIN = os.environ.get(
    "CURSOR_BIN", str(pathlib.Path.home() / ".local/bin/cursor-agent")
)


def utcnow() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


CURSOR_SHARE = pathlib.Path.home() / ".local/share/cursor-agent"
CURSOR_AUTH = pathlib.Path.home() / ".config/cursor/auth.json"

DOCKER_EXEC_SCRIPT = """\
export PATH="$(ls -d /root/.local/share/cursor-agent/versions/* | tail -1):$PATH"
source /opt/miniconda3/bin/activate testbed
cd /testbed
exec cursor-agent -p "$(cat /bench/prompt.txt)" \
  --output-format stream-json --model "$BENCH_MODEL" --force --workspace /testbed
"""


def docker(args, **kw):
    # errors="replace": agents sometimes touch non-UTF-8 files; a strict decode
    # of `git diff` output would abort the whole batch.
    return subprocess.run(
        ["docker", *args], capture_output=True, text=True, errors="replace", **kw
    )


def run_instance_docker(inst, run_dir, model, timeout, keep):
    iid = inst["instance_id"]
    image = inst.get("image")
    if not image:
        sys.exit(f"{iid}: no image field — re-run fetch/merge step")
    print(f"  RUN  {iid} ({inst['repo']}) [docker {image.split('/')[-1]}]", flush=True)

    benchdir = run_dir / f"{iid}.bench"
    benchdir.mkdir()
    (benchdir / "prompt.txt").write_text(
        run_agent.PROMPT_TEMPLATE.format(problem_statement=inst["problem_statement"])
    )
    # rw copy: cursor-agent may refresh the auth token during the run.
    auth_copy = benchdir / "auth.json"
    shutil.copy(CURSOR_AUTH, auth_copy)

    cname = f"cursor-bench-{iid}"
    docker(["rm", "-f", cname])
    r = docker([
        "run", "-d", "--name", cname,
        "-v", f"{CURSOR_SHARE}:/root/.local/share/cursor-agent:ro",
        "-v", f"{auth_copy}:/root/.config/cursor/auth.json",
        "-v", f"{benchdir}:/bench",
        image, "tail", "-f", "/dev/null",
    ])
    if r.returncode != 0:
        sys.exit(f"docker run failed for {iid}: {r.stderr}")

    raw = run_dir / f"{iid}.raw.jsonl"
    err = run_dir / f"{iid}.stderr.log"
    summary = run_dir / f"{iid}.json"

    started = utcnow()
    t0 = dt.datetime.now()
    try:
        with raw.open("w") as out, err.open("w") as errf:
            proc = subprocess.run(
                [
                    "timeout", "--kill-after=10s", str(timeout),
                    "docker", "exec", "-e", f"BENCH_MODEL={model}",
                    cname, "bash", "-c", DOCKER_EXEC_SCRIPT,
                ],
                stdout=out, stderr=errf, check=False,
            )
        proc_rc = proc.returncode
        if proc_rc in (124, 137):
            print(f"       WARN: {iid} hit timeout ({timeout}s) — killed")
        ended = utcnow()
        wall_ms = int((dt.datetime.now() - t0).total_seconds() * 1000)

        docker(["exec", "-w", "/testbed", cname, "git", "add", "-N", "."])
        patch = docker([
            "exec", "-w", "/testbed", cname, "git",
            "-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false",
            "diff", "--src-prefix=a/", "--dst-prefix=b/",
        ]).stdout
    finally:
        docker(["rm", "-f", cname])

    (run_dir / f"{iid}.patch").write_text(patch)
    subprocess.run(
        [
            sys.executable, str(EXTRACT), "cursor", str(raw), "-",
            iid, inst["repo"], model, model,
            started, ended, str(wall_ms), str(proc_rc), str(summary),
        ],
        check=False,
    )
    try:
        data = json.loads(summary.read_text())
    except Exception:
        data = {"task_id": iid}
    data["swebench_instance_id"] = iid
    data["patch_bytes"] = len(patch)
    data["empty_patch"] = not patch.strip()
    data["runner_mode"] = "docker"
    summary.write_text(json.dumps(data, indent=2))
    if not keep:
        shutil.rmtree(benchdir, ignore_errors=True)
    return patch


def run_instance(inst, run_dir, model, timeout, keep):
    iid = inst["instance_id"]
    print(f"  RUN  {iid} ({inst['repo']})", flush=True)
    workdir = run_agent.checkout(inst)
    prompt = run_agent.PROMPT_TEMPLATE.format(
        problem_statement=inst["problem_statement"]
    )

    raw = run_dir / f"{iid}.raw.jsonl"
    err = run_dir / f"{iid}.stderr.log"
    summary = run_dir / f"{iid}.json"

    started = utcnow()
    t0 = dt.datetime.now()
    with raw.open("w") as out, err.open("w") as errf:
        proc = subprocess.run(
            [
                "timeout", "--kill-after=10s", str(timeout),
                CURSOR_BIN, "-p", prompt,
                "--output-format", "stream-json", "--model", model, "--force",
                "--workspace", str(workdir),
            ],
            stdout=out, stderr=errf, cwd=workdir, check=False,
        )
    proc_rc = proc.returncode
    if proc_rc in (124, 137):
        print(f"       WARN: {iid} hit timeout ({timeout}s) — killed")
    ended = utcnow()
    wall_ms = int((dt.datetime.now() - t0).total_seconds() * 1000)

    patch = run_agent.collect_patch(workdir)
    (run_dir / f"{iid}.patch").write_text(patch)

    subprocess.run(
        [
            sys.executable, str(EXTRACT), "cursor", str(raw), "-",
            iid, inst["repo"], model, model,
            started, ended, str(wall_ms), str(proc_rc), str(summary),
        ],
        check=False,
    )
    try:
        data = json.loads(summary.read_text())
    except Exception:
        data = {"task_id": iid}
    data["swebench_instance_id"] = iid
    data["patch_bytes"] = len(patch)
    data["empty_patch"] = not patch.strip()
    summary.write_text(json.dumps(data, indent=2))

    if not keep:
        shutil.rmtree(workdir, ignore_errors=True)
    return patch


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True, help="cursor-agent model slug")
    ap.add_argument("--filter", default="", help="instance_id prefix filter")
    ap.add_argument(
        "--instance-ids", default="",
        help="comma-separated exact instance ids (overrides --filter)",
    )
    ap.add_argument("--tag", default="", help="label folded into run dir name")
    ap.add_argument("--timeout", type=int, default=900, help="per-instance seconds")
    ap.add_argument("--keep", action="store_true", help="keep worktrees after run")
    ap.add_argument(
        "--docker", action="store_true",
        help="run cursor-agent inside each instance's official SWE-bench image",
    )
    args = ap.parse_args()

    if not run_agent.INSTANCES.exists():
        sys.exit("instances.jsonl missing — run fetch_dataset.py first")
    if not os.access(CURSOR_BIN, os.X_OK):
        sys.exit(f"cursor-agent not found/executable at {CURSOR_BIN}")

    instances = [json.loads(ln) for ln in run_agent.INSTANCES.open()]
    if args.instance_ids:
        wanted = {s.strip() for s in args.instance_ids.split(",") if s.strip()}
        instances = [i for i in instances if i["instance_id"] in wanted]
        missing = wanted - {i["instance_id"] for i in instances}
        if missing:
            sys.exit(f"unknown instance ids: {sorted(missing)}")
    else:
        instances = [i for i in instances if i["instance_id"].startswith(args.filter)]
    if not instances:
        sys.exit("no instances selected")

    run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S") + "-swebench-cursor"
    if args.docker:
        run_id += "-docker"
        if not CURSOR_AUTH.exists():
            sys.exit(f"cursor auth not found at {CURSOR_AUTH} — run cursor-agent login")
    if args.tag:
        run_id += f"-{args.tag}"
    run_dir = run_agent.RESULTS_DIR / run_id
    run_dir.mkdir(parents=True)

    print(f"=== cursor-agent swe-bench runner ===\nModel: {args.model}")
    print(f"Run dir: {run_dir}\nInstances: {len(instances)}")
    print("NOTE: cursor bills Cursor's backend — token/correctness parity only.\n")

    preds_path = run_dir / "preds.jsonl"
    runner = run_instance_docker if args.docker else run_instance
    with preds_path.open("w") as preds:
        for inst in instances:
            patch = runner(inst, run_dir, args.model, args.timeout, args.keep)
            preds.write(json.dumps({
                "instance_id": inst["instance_id"],
                "model_name_or_path": f"cursor-{args.model}",
                "model_patch": patch,
            }) + "\n")
            preds.flush()

    print(f"\nPredictions: {preds_path}")


if __name__ == "__main__":
    main()
