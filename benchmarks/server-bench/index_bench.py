#!/usr/bin/env python3
"""Deterministic lifecycle benchmark for daimonos filename/path indexing.

Runs multiple release binaries and index modes against identical fixtures.
Each replicate starts a fresh daemon and records:

- socket-ready startup latency
- first filename-search latency and correctness
- time until a correct search result is available
- repeated warm-search latency
- process RSS, FD, and inotify snapshots
- returned index coverage metadata

Arm syntax:
    --arm LABEL=MODE=COMMIT=BINARY

Example:
    python3 index_bench.py \
      --arm baseline=legacy=53339f9=/tmp/baseline/daimonos \
      --arm hybrid=hybrid=cc6ecb3=target/release/daimonos
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path

from bench import Client, ResourceSnapshot, proc_resource_snapshot

HERE = Path(__file__).resolve().parent
RESULTS_DIR = HERE / "results"
FIND_OPCODE = 7
QUERY = "needle_target"
TARGET_FILE = "000_needle_target.rs"
STARTUP_TIMEOUT_S = 10.0


@dataclass(frozen=True)
class Arm:
    label: str
    mode: str
    commit: str
    binary: Path
    binary_sha256: str


@dataclass(frozen=True)
class Profile:
    name: str
    file_count: int
    marked: bool
    query: str


@dataclass
class Sample:
    startup_ns: int
    first_search_ns: int
    time_to_correct_ns: int | None
    first_search_correct: bool
    correct: bool
    readiness_calls: int
    warm_timings_ns: list[int]
    startup_resources: ResourceSnapshot
    ready_resources: ResourceSnapshot
    warm_resources: ResourceSnapshot
    index: dict


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_arm(value: str) -> Arm:
    parts = value.split("=", 3)
    if len(parts) != 4 or not all(parts):
        raise argparse.ArgumentTypeError(
            "arm must be LABEL=MODE=COMMIT=BINARY"
        )
    label, mode, commit, binary_text = parts
    if mode not in {"legacy", "eager", "lazy", "hybrid"}:
        raise argparse.ArgumentTypeError(
            f"unsupported mode {mode!r}; use legacy/eager/lazy/hybrid"
        )
    binary = Path(binary_text).expanduser().resolve()
    if not binary.is_file():
        raise argparse.ArgumentTypeError(f"binary not found: {binary}")
    return Arm(
        label=label,
        mode=mode,
        commit=commit,
        binary=binary,
        binary_sha256=sha256_file(binary),
    )


def percentile(values: list[int], percentile_value: float) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(
        0,
        min(
            len(ordered) - 1,
            round(percentile_value / 100.0 * (len(ordered) - 1)),
        ),
    )
    return ordered[index]


def timing_summary(values: list[int]) -> dict:
    if not values:
        return {"count": 0}
    return {
        "count": len(values),
        "min_ns": min(values),
        "max_ns": max(values),
        "mean_ns": int(statistics.mean(values)),
        "median_ns": int(statistics.median(values)),
        "p95_ns": percentile(values, 95),
        "p99_ns": percentile(values, 99),
        "stdev_ns": int(statistics.stdev(values)) if len(values) > 1 else 0,
    }


def response_is_correct(response: dict, query: str) -> bool:
    if not response.get("ok", False):
        return False
    results = response.get("d", {}).get("results", [])
    if query == QUERY:
        return any(result.get("file") == TARGET_FILE for result in results)
    return any(
        str(result.get("file", "")).endswith(query) for result in results
    )


def response_index(response: dict) -> dict:
    if not response.get("ok", False):
        return {}
    index = response.get("d", {}).get("index", {})
    return index if isinstance(index, dict) else {}


def max_resources(*snapshots: ResourceSnapshot) -> ResourceSnapshot:
    def maximum(field_name: str) -> int:
        values = [
            getattr(snapshot, field_name)
            for snapshot in snapshots
            if getattr(snapshot, field_name) >= 0
        ]
        return max(values) if values else -1

    return ResourceSnapshot(
        rss_kb=maximum("rss_kb"),
        fd_count=maximum("fd_count"),
        inotify_watches=maximum("inotify_watches"),
    )


def summarize_samples(samples: list[Sample]) -> dict:
    warm_timings = [
        timing
        for sample in samples
        for timing in sample.warm_timings_ns
    ]
    ready_timings = [
        sample.time_to_correct_ns
        for sample in samples
        if sample.time_to_correct_ns is not None
    ]
    peak_resources = [
        max_resources(
            sample.startup_resources,
            sample.ready_resources,
            sample.warm_resources,
        )
        for sample in samples
    ]
    coverage_counts: dict[str, int] = {}
    for sample in samples:
        coverage = str(sample.index.get("coverage", "legacy"))
        coverage_counts[coverage] = coverage_counts.get(coverage, 0) + 1
    return {
        "replicates": len(samples),
        "first_search_correct": sum(
            sample.first_search_correct for sample in samples
        ),
        "eventually_correct": sum(sample.correct for sample in samples),
        "startup": timing_summary([sample.startup_ns for sample in samples]),
        "first_search": timing_summary(
            [sample.first_search_ns for sample in samples]
        ),
        "time_to_correct": timing_summary(ready_timings),
        "warm_search": timing_summary(warm_timings),
        "peak_rss_kb": timing_summary(
            [
                resource.rss_kb
                for resource in peak_resources
                if resource.rss_kb >= 0
            ]
        ),
        "max_fd_count": max(
            (
                resource.fd_count
                for resource in peak_resources
                if resource.fd_count >= 0
            ),
            default=-1,
        ),
        "max_inotify_watches": max(
            (
                resource.inotify_watches
                for resource in peak_resources
                if resource.inotify_watches >= 0
            ),
            default=-1,
        ),
        "coverage_counts": coverage_counts,
    }


def create_fixture(root: Path, profile: Profile) -> Path:
    workspace = root / profile.name
    workspace.mkdir(parents=True)
    (workspace / TARGET_FILE).write_text(
        f"// {QUERY}\npub fn benchmark_target() {{}}\n"
    )
    for index in range(profile.file_count - 1):
        shard = workspace / f"shard-{index % 20:02d}"
        shard.mkdir(exist_ok=True)
        (shard / f"file_{index:06d}.rs").write_text(
            f"// synthetic file {index}\npub fn item_{index}() {{}}\n"
        )
    if profile.marked:
        (workspace / "Cargo.toml").write_text(
            "[package]\nname = \"index-bench\"\nversion = \"0.0.0\"\n"
        )
    return workspace


def write_config(
    path: Path,
    arm: Arm,
    max_files: int,
    max_walk_entries: int,
) -> None:
    lines = [
        "[index]",
        f"max_files = {max_files}",
        "max_depth = 20",
        "guard_overbroad_roots = true",
    ]
    if arm.mode != "legacy":
        lines.extend(
            [
                f'mode = "{arm.mode}"',
                f"max_walk_entries = {max_walk_entries}",
            ]
        )
    path.write_text("\n".join(lines) + "\n")


def wait_for_socket(
    proc: subprocess.Popen,
    socket_path: Path,
    started_ns: int,
) -> int:
    deadline = time.monotonic() + STARTUP_TIMEOUT_S
    while not socket_path.exists():
        if proc.poll() is not None:
            raise RuntimeError(
                f"daimonos exited during startup with {proc.returncode}"
            )
        if time.monotonic() >= deadline:
            raise RuntimeError(
                f"socket was not created within {STARTUP_TIMEOUT_S:.0f}s"
            )
        time.sleep(0.001)
    return time.perf_counter_ns() - started_ns


def timed_find(client: Client, query: str) -> tuple[int, dict]:
    started_ns = time.perf_counter_ns()
    response = client.call({"c": FIND_OPCODE, "p": query, "n": 20})
    return time.perf_counter_ns() - started_ns, response


def run_replicate(
    arm: Arm,
    workspace: Path,
    query: str,
    warm_calls: int,
    ready_timeout_s: float,
    max_files: int,
    max_walk_entries: int,
) -> Sample:
    run_dir = Path(tempfile.mkdtemp(prefix="daimonos-index-bench-run-"))
    socket_path = run_dir / "bench.sock"
    config_path = run_dir / "daimonos.toml"
    home = run_dir / "home"
    home.mkdir()
    write_config(config_path, arm, max_files, max_walk_entries)
    env = os.environ.copy()
    env["HOME"] = str(home)
    env.pop("DAIMONOS_CONFIG", None)
    proc: subprocess.Popen | None = None
    client: Client | None = None
    try:
        started_ns = time.perf_counter_ns()
        proc = subprocess.Popen(
            [
                str(arm.binary),
                "--socket",
                str(socket_path),
                "--workspace",
                str(workspace),
                "--config",
                str(config_path),
            ],
            cwd=workspace,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        startup_ns = wait_for_socket(proc, socket_path, started_ns)
        startup_resources = proc_resource_snapshot(proc.pid)
        client = Client(socket_path)

        ready_started_ns = time.perf_counter_ns()
        first_search_ns, response = timed_find(client, query)
        first_search_correct = response_is_correct(response, query)
        correct = first_search_correct
        readiness_calls = 1
        latest_response = response
        deadline = time.monotonic() + ready_timeout_s
        while not correct and time.monotonic() < deadline:
            time.sleep(0.005)
            _, latest_response = timed_find(client, query)
            readiness_calls += 1
            correct = response_is_correct(latest_response, query)
        time_to_correct_ns = (
            time.perf_counter_ns() - ready_started_ns if correct else None
        )
        ready_resources = proc_resource_snapshot(proc.pid)

        warm_timings_ns: list[int] = []
        if correct:
            for _ in range(warm_calls):
                elapsed_ns, warm_response = timed_find(client, query)
                if not response_is_correct(warm_response, query):
                    raise RuntimeError("warm filename search lost target")
                warm_timings_ns.append(elapsed_ns)
                latest_response = warm_response
        warm_resources = proc_resource_snapshot(proc.pid)
        return Sample(
            startup_ns=startup_ns,
            first_search_ns=first_search_ns,
            time_to_correct_ns=time_to_correct_ns,
            first_search_correct=first_search_correct,
            correct=correct,
            readiness_calls=readiness_calls,
            warm_timings_ns=warm_timings_ns,
            startup_resources=startup_resources,
            ready_resources=ready_resources,
            warm_resources=warm_resources,
            index=response_index(latest_response),
        )
    finally:
        if client is not None:
            client.close()
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        shutil.rmtree(run_dir, ignore_errors=True)


def format_ms(value_ns: int | None) -> str:
    return "—" if value_ns is None else f"{value_ns / 1_000_000:.2f}"


def render_markdown(payload: dict) -> str:
    lines = [
        "# Index lifecycle benchmark",
        "",
        f"- Generated: `{payload['generated_at']}`",
        f"- Host: `{payload['host']['node']}` / `{payload['host']['kernel']}`",
        f"- Replicates: {payload['replicates']}",
        f"- Warm calls per replicate: {payload['warm_calls']}",
        "",
    ]
    for profile in payload["profiles"]:
        lines.extend(
            [
                f"## {profile['profile']}",
                "",
                "| Arm | Mode | Correct | Startup median ms | First search median ms | Ready median ms | Warm median ms | Peak RSS median KiB | Coverage |",
                "|---|---:|---:|---:|---:|---:|---:|---:|---|",
            ]
        )
        for arm_result in profile["arms"]:
            summary = arm_result["summary"]
            lines.append(
                "| {label} | {mode} | {correct}/{replicates} | {startup} | "
                "{first} | {ready} | {warm} | {rss} | {coverage} |".format(
                    label=arm_result["label"],
                    mode=arm_result["mode"],
                    correct=summary["eventually_correct"],
                    replicates=summary["replicates"],
                    startup=format_ms(summary["startup"].get("median_ns")),
                    first=format_ms(
                        summary["first_search"].get("median_ns")
                    ),
                    ready=format_ms(
                        summary["time_to_correct"].get("median_ns")
                    ),
                    warm=format_ms(
                        summary["warm_search"].get("median_ns")
                    ),
                    rss=summary["peak_rss_kb"].get("median_ns", "—"),
                    coverage=", ".join(
                        f"{name}:{count}"
                        for name, count in sorted(
                            summary["coverage_counts"].items()
                        )
                    ),
                )
            )
        lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--arm",
        action="append",
        type=parse_arm,
        required=True,
        help="LABEL=MODE=COMMIT=BINARY; may be repeated",
    )
    parser.add_argument("--replicates", type=int, default=10)
    parser.add_argument("--warm-calls", type=int, default=20)
    parser.add_argument("--ready-timeout", type=float, default=2.0)
    parser.add_argument("--small-files", type=int, default=200)
    parser.add_argument("--large-files", type=int, default=3_000)
    parser.add_argument("--max-files", type=int, default=500)
    parser.add_argument("--max-walk-entries", type=int, default=1_000)
    parser.add_argument(
        "--profiles",
        nargs="+",
        choices=("small", "large-unmarked", "large-marked"),
        default=("small", "large-unmarked", "large-marked"),
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=None,
        help="Output directory (default: results/index-<timestamp>)",
    )
    args = parser.parse_args()
    if args.replicates < 1 or args.warm_calls < 1:
        parser.error("replicates and warm-calls must be positive")
    if args.max_walk_entries < args.max_files:
        parser.error("max-walk-entries must be >= max-files")
    labels = [arm.label for arm in args.arm]
    if len(labels) != len(set(labels)):
        parser.error("arm labels must be unique")

    profiles_by_name = {
        "small": Profile("small", args.small_files, False, QUERY),
        "large-unmarked": Profile(
            "large-unmarked", args.large_files, False, ".rs"
        ),
        "large-marked": Profile(
            "large-marked", args.large_files, True, ".rs"
        ),
    }
    out_dir = args.out or (
        RESULTS_DIR / f"index-{time.strftime('%Y%m%d-%H%M%S')}"
    )
    out_dir.mkdir(parents=True, exist_ok=True)
    fixture_root = Path(tempfile.mkdtemp(prefix="daimonos-index-bench-fixtures-"))
    try:
        profile_results = []
        for profile_name in args.profiles:
            profile = profiles_by_name[profile_name]
            print(
                f"index-bench: creating {profile.name} "
                f"fixture ({profile.file_count} files)",
                file=sys.stderr,
            )
            workspace = create_fixture(fixture_root, profile)
            samples_by_label: dict[str, list[Sample]] = {
                arm.label: [] for arm in args.arm
            }
            for replicate in range(args.replicates):
                # Rotate arm order each replicate so thermal/load drift does
                # not systematically favor the first or last arm.
                for offset in range(len(args.arm)):
                    arm = args.arm[(replicate + offset) % len(args.arm)]
                    samples_by_label[arm.label].append(
                        run_replicate(
                            arm,
                            workspace,
                            profile.query,
                            args.warm_calls,
                            args.ready_timeout,
                            args.max_files,
                            args.max_walk_entries,
                        )
                    )
            arm_results = []
            for arm in args.arm:
                print(
                    f"index-bench: {profile.name}/{arm.label} "
                    f"({arm.mode}) completed × {args.replicates}",
                    file=sys.stderr,
                )
                samples = samples_by_label[arm.label]
                arm_results.append(
                    {
                        "label": arm.label,
                        "mode": arm.mode,
                        "commit": arm.commit,
                        "binary": str(arm.binary),
                        "binary_sha256": arm.binary_sha256,
                        "samples": [asdict(sample) for sample in samples],
                        "summary": summarize_samples(samples),
                    }
                )
            profile_results.append(
                {
                    "profile": profile.name,
                    "file_count": profile.file_count,
                    "marked": profile.marked,
                    "query": profile.query,
                    "arms": arm_results,
                }
            )

        payload = {
            "schema_version": 1,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "harness_sha256": sha256_file(Path(__file__)),
            "host": {
                "node": platform.node(),
                "kernel": platform.release(),
                "python": platform.python_version(),
            },
            "replicates": args.replicates,
            "warm_calls": args.warm_calls,
            "ready_timeout_s": args.ready_timeout,
            "limits": {
                "max_files": args.max_files,
                "max_walk_entries": args.max_walk_entries,
            },
            "profiles": profile_results,
        }
        results_file = out_dir / "results.json"
        report_file = out_dir / "report.md"
        results_file.write_text(json.dumps(payload, indent=2))
        report_file.write_text(render_markdown(payload))
        print(f"index-bench: wrote {results_file}", file=sys.stderr)
        print(f"index-bench: wrote {report_file}", file=sys.stderr)
        return 0
    finally:
        shutil.rmtree(fixture_root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
