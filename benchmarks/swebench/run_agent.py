#!/usr/bin/env python3
"""SWE-bench runner for daimonos-as-agent.

For each instance: fresh checkout of the repo at base_commit, one
`daimonos agent` invocation with the issue text, then `git diff` becomes the
prediction. Token/cost accounting reuses ../extract_tokens.py against the
--debug-tokens log delta, so analyze.py works on the per-instance JSONs.

Usage:
  .venv/bin/python run_agent.py [--filter PREFIX] [--tag TAG]
                                [--model SLUG] [--timeout SECS] [--keep]

Model/provider/key come from ~/.config/daimonos/agent.env (override file with
DAIMONOS_AGENT_ENV), same as bench-agent.sh. The runner copies it with
APPROVAL_MODE=auto and COMPACTION=off for run parity with the in-house suite.
"""
import argparse
import datetime as dt
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).parent
BENCH_ROOT = HERE.parent
INSTANCES = HERE / "instances.jsonl"
REPOS_DIR = HERE / "repos"
WORK_DIR = HERE / "worktrees"
RESULTS_DIR = HERE / "results"
DAIMONOS_BIN = pathlib.Path(
    os.environ.get("DAIMONOS_BIN", BENCH_ROOT.parent / "target/release/daimonos")
)
TOKEN_LOG = pathlib.Path.home() / ".config/daimonos/token-debug.log"
EXTRACT = BENCH_ROOT / "extract_tokens.py"

PROMPT_TEMPLATE = """You are working in a checked-out git repository (the current \
workspace root). Solve the following GitHub issue by modifying the source code.

<issue>
{problem_statement}
</issue>

Requirements:
- Fix the underlying problem, not just the symptom in the issue text.
- Do NOT modify any test files or add new tests; the fix is verified by an \
external test harness.
- Make the minimal change that resolves the issue.
- Ensure all edits are saved to disk before you finish. Do not commit.
"""


def sh(args, **kw):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kw)


def utcnow() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def make_bench_env(src: pathlib.Path) -> pathlib.Path:
    lines = [
        ln
        for ln in src.read_text().splitlines()
        if not ln.startswith(("DAIMONOS_AGENT_APPROVAL_MODE=", "DAIMONOS_AGENT_COMPACTION="))
    ]
    lines += ["DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_COMPACTION=off"]
    fd, path = tempfile.mkstemp(prefix="daimonos-swebench-env.")
    with os.fdopen(fd, "w") as f:
        f.write("\n".join(lines) + "\n")
    os.chmod(path, 0o600)
    return pathlib.Path(path)


def repo_cache(repo: str) -> pathlib.Path:
    cache = REPOS_DIR / (repo.replace("/", "__") + ".git")
    if not cache.exists():
        print(f"    cloning cache for {repo} ...")
        REPOS_DIR.mkdir(parents=True, exist_ok=True)
        sh(["git", "clone", "--bare", f"https://github.com/{repo}.git", str(cache)])
    return cache


def checkout(inst: dict) -> pathlib.Path:
    cache = repo_cache(inst["repo"])
    workdir = WORK_DIR / inst["instance_id"]
    if workdir.exists():
        shutil.rmtree(workdir)
    WORK_DIR.mkdir(parents=True, exist_ok=True)
    # Local clone hardlinks objects: fast and cheap.
    sh(["git", "clone", "--quiet", str(cache), str(workdir)])
    sh(["git", "-C", str(workdir), "checkout", "--quiet", inst["base_commit"]])
    return workdir


def collect_patch(workdir: pathlib.Path) -> str:
    # Include untracked files the agent may have added (intent-to-add).
    sh(["git", "-C", str(workdir), "add", "-N", "."])
    # Force standard a/-b/ prefixes: the SWE-bench applier rejects the
    # mnemonic i/-w/ prefixes a user gitconfig may enable.
    return subprocess.run(
        [
            "git", "-C", str(workdir),
            "-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false",
            "diff", "--src-prefix=a/", "--dst-prefix=b/",
        ],
        check=True, capture_output=True, text=True,
    ).stdout


def token_log_offset() -> int:
    if TOKEN_LOG.exists():
        return len(TOKEN_LOG.read_bytes().splitlines())
    return 0


MUSL_BIN = pathlib.Path(
    os.environ.get(
        "DAIMONOS_MUSL_BIN",
        BENCH_ROOT.parent / "target/x86_64-unknown-linux-musl/release/daimonos",
    )
)

# Instance images ship glibc 2.35; the host build needs 2.38+, hence musl.
# The agent runs inside the official SWE-bench instance container at /testbed
# with the `testbed` conda env active, so it can actually execute the project's
# test suite — parity with SWE-agent / mini-swe-agent setups.
DOCKER_EXEC_SCRIPT = """\
source /opt/miniconda3/bin/activate testbed
exec daimonos --debug-tokens -w /testbed agent "$(cat /bench/prompt.txt)" \
  --model "$BENCH_MODEL" --agent-env /bench/agent.env
"""


def docker(args, **kw):
    # errors="replace": agents sometimes touch non-UTF-8 files; a strict decode
    # of `git diff` output would abort the whole batch.
    return subprocess.run(
        ["docker", *args], capture_output=True, text=True, errors="replace", **kw
    )


def run_instance_docker(inst, run_dir, bench_env, model, timeout, keep):
    iid = inst["instance_id"]
    image = inst.get("image")
    if not image:
        sys.exit(f"{iid}: no image field — re-run fetch/merge step")
    print(f"  RUN  {iid} ({inst['repo']}) [docker {image.split('/')[-1]}]")

    benchdir = run_dir / f"{iid}.bench"
    benchdir.mkdir()
    shutil.copy(bench_env, benchdir / "agent.env")
    (benchdir / "prompt.txt").write_text(
        PROMPT_TEMPLATE.format(problem_statement=inst["problem_statement"])
    )
    tokenhome = benchdir / "confighome"
    tokenhome.mkdir()

    cname = f"daimonos-bench-{iid}"
    docker(["rm", "-f", cname])
    r = docker([
        "run", "-d", "--name", cname,
        "-v", f"{MUSL_BIN}:/usr/local/bin/daimonos:ro",
        "-v", f"{benchdir}:/bench:ro",
        "-v", f"{tokenhome}:/root/.config/daimonos",
        image, "tail", "-f", "/dev/null",
    ])
    if r.returncode != 0:
        sys.exit(f"docker run failed for {iid}: {r.stderr}")

    raw = run_dir / f"{iid}.raw.txt"
    err = run_dir / f"{iid}.stderr.log"
    tokenlog = run_dir / f"{iid}.tokenlog.jsonl"
    summary = run_dir / f"{iid}.json"

    started = utcnow()
    t0 = dt.datetime.now()
    try:
        with raw.open("w") as out, err.open("w") as errf:
            proc = subprocess.run(
                [
                    "timeout", "--kill-after=10s", str(timeout),
                    "docker", "exec", "-w", "/testbed",
                    "-e", f"BENCH_MODEL={model}",
                    cname, "bash", "-c", DOCKER_EXEC_SCRIPT,
                ],
                stdout=out, stderr=errf, check=False,
            )
        proc_rc = proc.returncode
        if proc_rc in (124, 137):
            print(f"       WARN: {iid} hit timeout ({timeout}s) — killed")
        ended = utcnow()
        wall_ms = int((dt.datetime.now() - t0).total_seconds() * 1000)

        container_log = tokenhome / "token-debug.log"
        if container_log.exists():
            shutil.copy(container_log, tokenlog)
        else:
            tokenlog.write_text("")

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
            sys.executable, str(EXTRACT), "daimonos", str(raw), str(tokenlog),
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


def run_instance(inst, run_dir, bench_env, model, timeout, keep):
    iid = inst["instance_id"]
    print(f"  RUN  {iid} ({inst['repo']})")
    workdir = checkout(inst)
    prompt = PROMPT_TEMPLATE.format(problem_statement=inst["problem_statement"])

    raw = run_dir / f"{iid}.raw.txt"
    err = run_dir / f"{iid}.stderr.log"
    tokenlog = run_dir / f"{iid}.tokenlog.jsonl"
    summary = run_dir / f"{iid}.json"

    pre_lines = token_log_offset()
    started = utcnow()
    t0 = dt.datetime.now()
    try:
        with raw.open("w") as out, err.open("w") as errf:
            proc = subprocess.run(
                [
                    "timeout", "--kill-after=10s", str(timeout),
                    str(DAIMONOS_BIN), "--debug-tokens", "-w", str(workdir),
                    "agent", prompt, "--model", model,
                    "--agent-env", str(bench_env),
                ],
                stdout=out, stderr=errf, cwd=workdir, check=False,
            )
        proc_rc = proc.returncode
        if proc_rc in (124, 137):
            print(f"       WARN: {iid} hit timeout ({timeout}s) — killed")
    except Exception as e:  # spawn failure, not task failure
        err.write_text(str(e))
        proc_rc = -1
    ended = utcnow()
    wall_ms = int((dt.datetime.now() - t0).total_seconds() * 1000)

    if TOKEN_LOG.exists():
        lines = TOKEN_LOG.read_bytes().splitlines()[pre_lines:]
        tokenlog.write_bytes(b"\n".join(lines) + (b"\n" if lines else b""))
    else:
        tokenlog.write_text("")

    patch = collect_patch(workdir)
    (run_dir / f"{iid}.patch").write_text(patch)

    subprocess.run(
        [
            sys.executable, str(EXTRACT), "daimonos", str(raw), str(tokenlog),
            iid, inst["repo"], model, model,
            started, ended, str(wall_ms), str(proc_rc), str(summary),
        ],
        check=False,
    )
    # Stamp SWE-bench fields into the summary for later analysis.
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
    ap.add_argument("--filter", default="", help="instance_id prefix filter")
    ap.add_argument(
        "--instance-ids", default="",
        help="comma-separated exact instance ids (overrides --filter)",
    )
    ap.add_argument("--tag", default="", help="label folded into run dir name")
    ap.add_argument("--model", default=None, help="override agent.env model")
    ap.add_argument("--timeout", type=int, default=900, help="per-instance seconds")
    ap.add_argument("--keep", action="store_true", help="keep worktrees after run")
    ap.add_argument(
        "--docker", action="store_true",
        help="run the agent inside each instance's official SWE-bench image "
             "(runnable test env; requires the musl daimonos build)",
    )
    args = ap.parse_args()

    if not INSTANCES.exists():
        sys.exit("instances.jsonl missing — run fetch_dataset.py first")
    if args.docker:
        if not MUSL_BIN.exists():
            sys.exit(f"musl daimonos binary not found at {MUSL_BIN} — "
                     "cargo build --release --target x86_64-unknown-linux-musl")
    elif not DAIMONOS_BIN.exists():
        sys.exit(f"daimonos binary not found at {DAIMONOS_BIN}")

    src_env = pathlib.Path(
        os.environ.get(
            "DAIMONOS_AGENT_ENV", pathlib.Path.home() / ".config/daimonos/agent.env"
        )
    )
    if not src_env.exists():
        sys.exit(f"no agent env at {src_env}")

    model = args.model
    if model is None:
        for ln in src_env.read_text().splitlines():
            if ln.startswith("DAIMONOS_AGENT_MODEL="):
                model = ln.split("=", 1)[1].strip().strip('"')
    if not model:
        sys.exit("no model in agent.env and no --model given")

    instances = [json.loads(ln) for ln in INSTANCES.open()]
    if args.instance_ids:
        wanted = {s.strip() for s in args.instance_ids.split(",") if s.strip()}
        instances = [i for i in instances if i["instance_id"] in wanted]
        missing = wanted - {i["instance_id"] for i in instances}
        if missing:
            sys.exit(f"unknown instance ids: {sorted(missing)}")
    else:
        instances = [i for i in instances if i["instance_id"].startswith(args.filter)]
    if not instances:
        sys.exit(f"no instances match filter {args.filter!r}")

    run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S") + "-swebench"
    if args.docker:
        run_id += "-docker"
    if args.tag:
        run_id += f"-{args.tag}"
    run_dir = RESULTS_DIR / run_id
    run_dir.mkdir(parents=True)

    bench_env = make_bench_env(src_env)
    print(f"=== daimonos swe-bench runner ===\nModel: {model}\nRun dir: {run_dir}")
    print(f"Instances: {len(instances)}\n")

    preds_path = run_dir / "preds.jsonl"
    try:
        runner = run_instance_docker if args.docker else run_instance
        with preds_path.open("w") as preds:
            for inst in instances:
                patch = runner(
                    inst, run_dir, bench_env, model, args.timeout, args.keep
                )
                preds.write(json.dumps({
                    "instance_id": inst["instance_id"],
                    "model_name_or_path": f"daimonos-{model}",
                    "model_patch": patch,
                }) + "\n")
                preds.flush()
    finally:
        bench_env.unlink(missing_ok=True)

    print(f"\nPredictions: {preds_path}")
    print("Evaluate with (needs docker):")
    print(
        f"  .venv/bin/python -m swebench.harness.run_evaluation \\\n"
        f"    --dataset_name SWE-bench/SWE-bench_Verified \\\n"
        f"    --predictions_path {preds_path} \\\n"
        f"    --max_workers 2 --run_id {run_id}"
    )


if __name__ == "__main__":
    main()
