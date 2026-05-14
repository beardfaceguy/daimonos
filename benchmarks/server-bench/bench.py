#!/usr/bin/env python3
"""Server-side deterministic benchmark for daimonos.

Spawns a single `daimonos` socket-mode daemon, connects to it over the Unix
socket, and runs each task module's opcode sequence M times — recording
wall-clock time for every individual opcode round-trip, plus end-of-run
resource snapshots (RSS, FD count, inotify watch count on Linux).

This harness is the deterministic counterpart to the LLM-driven
benchmarks in `benchmarks/`: it bypasses any model entirely so its
variance reflects only daimonos itself. Useful for catching small
server-side regressions that LLM-strategy variance would otherwise
bury (see Vikunja #264 for motivation).

Usage:
    python3 bench.py                      # run all tasks, N=20 each
    python3 bench.py --replicates 50      # more samples
    python3 bench.py --tasks read_100     # subset
    python3 bench.py --out results/foo    # custom output dir
"""

from __future__ import annotations

import argparse
import importlib
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, asdict, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
TASKS_DIR = HERE / "tasks"
RESULTS_DIR = HERE / "results"

DEFAULT_TASKS = ["read_100", "search_many", "snapshot_cycle", "exec_burst"]
DEFAULT_REPLICATES = 20

# How long we wait for the daimonos socket to appear after Popen. The
# daemon binds before accepting any connections, and on this machine
# the socket shows up within a few tens of milliseconds — 10 s is
# generous enough that genuine slow-start cases (cold disk cache,
# heavily loaded CI machine) clear with margin but tight enough that a
# wedged daemon can't hang the harness indefinitely.
STARTUP_TIMEOUT_S = 10.0


@dataclass
class CallSample:
    """One opcode round-trip timing record."""

    op_code: int
    elapsed_ns: int


@dataclass
class ResourceSnapshot:
    """End-of-run process resource snapshot from /proc.

    All fields are -1 on platforms that lack the relevant /proc entry
    (i.e. anything that isn't Linux), so consumers can treat them as
    optional without special-casing the platform check.
    """

    rss_kb: int = -1
    fd_count: int = -1
    inotify_watches: int = -1


@dataclass
class TaskResult:
    task_id: str
    description: str
    iterations: int
    timings_ns: list[int] = field(default_factory=list)
    op_codes: list[int] = field(default_factory=list)
    resources: ResourceSnapshot = field(default_factory=ResourceSnapshot)

    def summary(self) -> dict:
        if not self.timings_ns:
            return {"count": 0}
        n = len(self.timings_ns)
        sorted_t = sorted(self.timings_ns)

        def percentile(p: float) -> int:
            # Nearest-rank percentile; matches what most ops benchmarks
            # quote (and matches numpy's `method='nearest'`). Avoids the
            # interpolated variants that can return a value never
            # actually observed.
            k = max(0, min(n - 1, int(round(p / 100.0 * (n - 1)))))
            return sorted_t[k]

        return {
            "count": n,
            "min_ns": sorted_t[0],
            "max_ns": sorted_t[-1],
            "mean_ns": int(statistics.mean(sorted_t)),
            "median_ns": int(statistics.median(sorted_t)),
            "p95_ns": percentile(95),
            "p99_ns": percentile(99),
            "stdev_ns": int(statistics.stdev(sorted_t)) if n > 1 else 0,
        }


def find_binary() -> Path:
    """Locate the release daimonos binary, building it if necessary.

    Falls back to the debug binary if release isn't built — useful for
    fast iteration on the harness itself. Real benchmark runs should
    always go through release; the harness warns when it doesn't.
    """
    release = REPO_ROOT / "target" / "release" / "daimonos"
    if release.exists():
        return release
    debug = REPO_ROOT / "target" / "debug" / "daimonos"
    if debug.exists():
        print(
            "warning: using debug build — release build not found. "
            "Run `cargo build --release` for representative numbers.",
            file=sys.stderr,
        )
        return debug
    print(
        "no daimonos binary found; building release...",
        file=sys.stderr,
    )
    subprocess.run(
        ["cargo", "build", "--release"],
        cwd=REPO_ROOT,
        check=True,
    )
    return release


def proc_resource_snapshot(pid: int) -> ResourceSnapshot:
    """Sample RSS, FD count, and inotify watch count from /proc.

    Linux-only; returns -1 fields on other platforms. We intentionally
    sample only at end-of-run rather than per-op — sampling /proc is
    O(N_fd) and would skew per-op timings if done in the hot path.
    """
    if not Path("/proc").is_dir():
        return ResourceSnapshot()
    try:
        status_text = Path(f"/proc/{pid}/status").read_text()
    except (FileNotFoundError, PermissionError):
        return ResourceSnapshot()
    rss_kb = -1
    for line in status_text.splitlines():
        if line.startswith("VmRSS:"):
            parts = line.split()
            if len(parts) >= 2:
                try:
                    rss_kb = int(parts[1])
                except ValueError:
                    pass
            break
    fd_dir = Path(f"/proc/{pid}/fd")
    try:
        fd_count = sum(1 for _ in fd_dir.iterdir())
    except (FileNotFoundError, PermissionError):
        fd_count = -1
    fdinfo_dir = Path(f"/proc/{pid}/fdinfo")
    inotify_watches = 0
    found_any = False
    try:
        for entry in fdinfo_dir.iterdir():
            try:
                text = entry.read_text()
            except (FileNotFoundError, PermissionError, OSError):
                continue
            for line in text.splitlines():
                if line.startswith("inotify wd:"):
                    inotify_watches += 1
                    found_any = True
    except (FileNotFoundError, PermissionError):
        return ResourceSnapshot(rss_kb=rss_kb, fd_count=fd_count, inotify_watches=-1)
    if not found_any and inotify_watches == 0:
        # No inotify fd at all is legitimate (0), not unsupported (-1).
        pass
    return ResourceSnapshot(
        rss_kb=rss_kb, fd_count=fd_count, inotify_watches=inotify_watches
    )


class Client:
    """Minimal line-delimited-JSON client for the daimonos Unix socket.

    Synchronous to keep per-call timing as clean as possible — we
    explicitly do not want asyncio scheduling jitter contaminating the
    measurement of an opcode round-trip.
    """

    def __init__(self, socket_path: Path) -> None:
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.connect(str(socket_path))
        self._sock.settimeout(30.0)
        self._buf = b""

    def call(self, op: dict) -> dict:
        """Send one op, await its single-line JSON response. Returns the
        decoded response dict (the caller is responsible for checking
        `ok`)."""
        payload = (json.dumps(op) + "\n").encode("utf-8")
        self._sock.sendall(payload)
        while b"\n" not in self._buf:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise RuntimeError("daimonos socket closed mid-response")
            self._buf += chunk
        line, _, rest = self._buf.partition(b"\n")
        self._buf = rest
        return json.loads(line.decode("utf-8"))

    def close(self) -> None:
        try:
            self._sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self._sock.close()


def load_task(task_id: str):
    """Import a task module by id and return the module object.

    Every task module must export: ID (str), DESCRIPTION (str),
    setup(workspace: Path) -> None, and
    run_iteration(client, workspace) -> list[(op_code: int, elapsed_ns: int)].

    `run_iteration` does its own per-call timing so dynamic tasks
    (snapshot cycle, anything where one op's input depends on a
    previous op's output) can be expressed uniformly with static
    sequences. The harness simply invokes it once per replicate.
    """
    sys.path.insert(0, str(HERE))
    try:
        mod = importlib.import_module(f"tasks.{task_id}")
    finally:
        sys.path.pop(0)
    required = ("ID", "DESCRIPTION", "setup", "run_iteration")
    missing = [attr for attr in required if not hasattr(mod, attr)]
    if missing:
        raise RuntimeError(
            f"task module 'tasks.{task_id}' missing required attrs: {missing}"
        )
    return mod


def run_task(
    task_mod,
    binary: Path,
    iterations: int,
    keep_workspace: bool = False,
) -> TaskResult:
    """Spawn a fresh daimonos for this task, point it at a clean
    workspace, run the iterations, tear it all down.

    Each task gets its own daemon + workspace so opcodes that operate
    on the whole workspace root (snap/restore especially) see only what
    setup() put there. Mixing tasks into one shared daemon
    cross-contaminates snapshot payloads with files written by earlier
    tasks, making results depend on task-list order rather than purely
    on server behavior.

    Cleanup invariant: any path that returns/raises after `mkdtemp` must
    go through the finally block so the tempdir and the daemon process
    can never be leaked, even when the daemon failed to start. Pass
    `keep_workspace=True` to preserve the tempdir for post-mortem
    inspection — useful when a task panics mid-run.
    """
    workspace = Path(tempfile.mkdtemp(prefix=f"daimonos-bench-{task_mod.ID}-"))
    socket_path = workspace / "bench.sock"
    env = os.environ.copy()
    env["HOME"] = str(workspace)
    env.pop("DAIMONOS_CONFIG", None)
    proc: subprocess.Popen | None = None
    client: Client | None = None
    try:
        proc = subprocess.Popen(
            [
                str(binary),
                "--socket",
                str(socket_path),
                "--workspace",
                str(workspace),
            ],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            cwd=str(workspace),
        )
        deadline = time.time() + STARTUP_TIMEOUT_S
        while not socket_path.exists():
            if proc.poll() is not None:
                err = proc.stderr.read() if proc.stderr else b""
                raise RuntimeError(
                    f"daimonos exited early for task {task_mod.ID}: "
                    f"{err.decode(errors='replace')!r}"
                )
            if time.time() >= deadline:
                raise RuntimeError(
                    f"daimonos did not create socket for task "
                    f"{task_mod.ID} within {STARTUP_TIMEOUT_S:.0f} s"
                )
            time.sleep(0.025)

        task_mod.setup(workspace)
        client = Client(socket_path)
        result = TaskResult(
            task_id=task_mod.ID,
            description=task_mod.DESCRIPTION,
            iterations=iterations,
        )
        # Warm-up iteration: not recorded. Discards the first run's
        # caches-cold penalty so we measure steady-state.
        task_mod.run_iteration(client, workspace)
        for _ in range(iterations):
            for op_code, elapsed_ns in task_mod.run_iteration(client, workspace):
                result.timings_ns.append(int(elapsed_ns))
                result.op_codes.append(int(op_code))
        result.resources = proc_resource_snapshot(proc.pid)
        return result
    finally:
        if client is not None:
            client.close()
        if proc is not None:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5.0)
            else:
                # Daemon already exited (early-fail path). Still call
                # wait() so the OS reaps the zombie and Python closes
                # the stderr PIPE deterministically rather than waiting
                # for GC.
                proc.wait(timeout=5.0)
        if keep_workspace:
            print(
                f"bench: kept workspace for {task_mod.ID}: {workspace}",
                file=sys.stderr,
            )
        else:
            shutil.rmtree(workspace, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tasks",
        nargs="+",
        default=DEFAULT_TASKS,
        help="Task ids to run (default: all)",
    )
    parser.add_argument(
        "--replicates",
        type=int,
        default=DEFAULT_REPLICATES,
        help=f"Iterations per task (default {DEFAULT_REPLICATES})",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=None,
        help="Output directory (default: results/<timestamp>/)",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=None,
        help="Path to daimonos binary (default: auto-detect)",
    )
    parser.add_argument(
        "--keep-workspace",
        action="store_true",
        help=(
            "Don't delete the per-task temp workspaces after each task "
            "finishes (the daemon is still terminated as normal — this "
            "preserves files for inspection, not a live daemon)"
        ),
    )
    args = parser.parse_args()

    binary = args.binary or find_binary()
    if not binary.exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 1

    out_dir = args.out or (RESULTS_DIR / time.strftime("%Y%m%d-%H%M%S"))
    out_dir.mkdir(parents=True, exist_ok=True)
    print(f"bench: binary={binary}", file=sys.stderr)
    print(f"bench: out={out_dir}", file=sys.stderr)

    all_results: list[TaskResult] = []
    for task_id in args.tasks:
        task_mod = load_task(task_id)
        print(
            f"bench: running {task_id} ({task_mod.DESCRIPTION}) "
            f"× {args.replicates} replicates",
            file=sys.stderr,
        )
        result = run_task(
            task_mod, binary, args.replicates, keep_workspace=args.keep_workspace
        )
        all_results.append(result)
        summary = result.summary()
        print(
            f"  → {summary['count']} calls, "
            f"median={summary['median_ns']/1000:.1f}µs, "
            f"p99={summary['p99_ns']/1000:.1f}µs, "
            f"stdev={summary['stdev_ns']/1000:.1f}µs",
            file=sys.stderr,
        )

    out_payload = {
        "binary": str(binary),
        "replicates": args.replicates,
        "tasks": [{**asdict(r), "summary": r.summary()} for r in all_results],
    }
    out_file = out_dir / "results.json"
    out_file.write_text(json.dumps(out_payload, indent=2))
    print(f"bench: wrote {out_file}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
