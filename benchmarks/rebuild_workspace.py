#!/usr/bin/env python3
"""Recreate the gitignored inventory-app benchmark fixture deterministically."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
TEMPLATE = SCRIPT_DIR / "workspace-template"
DEFAULT_DESTINATION = SCRIPT_DIR / "workspace"
FINGERPRINT_FILE = SCRIPT_DIR / "workspace-template.commit"
SENTINEL = ".daimonos-benchmark-fixture"
SENTINEL_CONTENT = "daimonos-benchmark-fixture-v1"
HISTORY = [
    "Bootstrap benchmark history",
    "Add project README",
    "Initial project setup",
    "Add inventory data",
    "Add configuration",
    "Add gitignore",
]
FINAL_SUBJECT = "Initial benchmark workspace"


def git_environment(commit_number: int | None) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("GIT_")
    }
    env.update(
        {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Daimonos Benchmark",
            "GIT_AUTHOR_EMAIL": "benchmark@daimonos.local",
            "GIT_COMMITTER_NAME": "Daimonos Benchmark",
            "GIT_COMMITTER_EMAIL": "benchmark@daimonos.local",
            "LC_ALL": "C",
            "TZ": "UTC",
        }
    )
    if commit_number is not None:
        timestamp = f"2026-01-01T00:00:{commit_number:02d}+00:00"
        env["GIT_AUTHOR_DATE"] = timestamp
        env["GIT_COMMITTER_DATE"] = timestamp
    return env


def git(workspace: Path, *args: str, commit_number: int | None = None) -> None:
    subprocess.run(
        ["git", *args],
        cwd=workspace,
        env=git_environment(commit_number),
        check=True,
        stdout=subprocess.DEVNULL,
    )


def safe_destination(raw_destination: Path) -> Path:
    destination = Path(os.path.abspath(raw_destination.expanduser()))
    if destination.is_symlink():
        raise SystemExit(f"refusing to replace symlink destination: {destination}")
    return destination


def validate_existing_fixture(destination: Path) -> None:
    sentinel = destination / SENTINEL
    is_fixture = (
        (destination / ".git").is_dir()
        and sentinel.is_file()
        and sentinel.read_text().strip() == SENTINEL_CONTENT
    )
    if not is_fixture:
        raise SystemExit(
            f"refusing to replace non-fixture directory: {destination}"
        )


def normalize_template_modes(destination: Path) -> None:
    for path in destination.rglob("*"):
        if path.is_symlink():
            raise SystemExit(f"fixture template must not contain symlinks: {path}")
        if path.is_file():
            path.chmod(0o644)


def rebuild(destination: Path, force: bool) -> str:
    destination = safe_destination(destination)
    if destination.exists():
        if not force:
            raise SystemExit(
                f"{destination} already exists; pass --force to replace it"
            )
        validate_existing_fixture(destination)
        shutil.rmtree(destination)

    shutil.copytree(TEMPLATE, destination)
    normalize_template_modes(destination)
    git(destination, "init", "-q", "--object-format=sha1", "-b", "master")
    git(destination, "config", "user.name", "Daimonos Benchmark")
    git(destination, "config", "user.email", "benchmark@daimonos.local")
    git(destination, "config", "commit.gpgSign", "false")
    git(destination, "config", "core.autocrlf", "false")

    # Tasks 06 and 09 intentionally measure a seven-commit history. The first
    # six commits are stable markers; the final commit owns the complete
    # fixture, so `git diff --stat HEAD~1` always describes its source/config.
    for number, subject in enumerate(HISTORY, start=1):
        git(
            destination,
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            subject,
            commit_number=number,
        )

    git(destination, "add", ".")
    git(
        destination,
        "commit",
        "-q",
        "-m",
        FINAL_SUBJECT,
        commit_number=len(HISTORY) + 1,
    )
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=destination,
        check=True,
        capture_output=True,
        text=True,
        env=git_environment(None),
    ).stdout.strip()
    expected = FINGERPRINT_FILE.read_text().strip()
    if commit != expected:
        raise SystemExit(
            f"fixture fingerprint mismatch: generated {commit}, expected {expected}"
        )
    return commit


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--destination",
        type=Path,
        default=DEFAULT_DESTINATION,
        help="workspace to create (default: benchmarks/workspace)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="replace an existing destination",
    )
    args = parser.parse_args()
    commit = rebuild(args.destination, args.force)
    print(f"Rebuilt {args.destination.resolve()}")
    print(f"Fixture commit: {commit}")


if __name__ == "__main__":
    main()
