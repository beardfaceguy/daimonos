"""Regression tests for the reproducible agent-benchmark workspace."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[1]
BUILDER = REPO_ROOT / "benchmarks" / "rebuild_workspace.py"
EXPECTED_FIXTURE_COMMIT = (
    REPO_ROOT / "benchmarks" / "workspace-template.commit"
).read_text().strip()
CARGO = shutil.which("cargo")
HAS_CLIPPY = bool(
    CARGO
    and subprocess.run(
        [CARGO, "clippy", "--version"],
        capture_output=True,
        timeout=30,
    ).returncode
    == 0
)


def run(
    *args: str,
    cwd: Path,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [*args],
        cwd=cwd,
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    )


def cargo_environment(**overrides: str) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if key not in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"}
    }
    env.update(overrides)
    return env


def test_rebuild_workspace_restores_benchmark_contract(tmp_path):
    workspace = tmp_path / "workspace"
    run(
        sys.executable,
        str(BUILDER),
        "--destination",
        str(workspace),
        cwd=REPO_ROOT,
    )

    assert {
        path.relative_to(workspace).as_posix()
        for path in (workspace / "src").glob("*.rs")
    } == {
        "src/config.rs",
        "src/inventory.rs",
        "src/main.rs",
        "src/report.rs",
    }
    assert (workspace / "data" / "inventory.csv").is_file()
    assert (workspace / "src" / "config.rs").read_text().count("category_labels") == 3

    assert run("git", "rev-list", "--count", "HEAD", cwd=workspace).stdout.strip() == "7"
    assert (
        run("git", "rev-parse", "HEAD", cwd=workspace).stdout.strip()
        == EXPECTED_FIXTURE_COMMIT
    )
    assert (
        run("git", "log", "-1", "--format=%s", cwd=workspace).stdout.strip()
        == "Initial benchmark workspace"
    )
    recent_subjects = run(
        "git", "log", "-5", "--format=%s", cwd=workspace
    ).stdout.splitlines()
    assert "Add inventory data" in recent_subjects
    assert "Add configuration" in recent_subjects
    changed_at_head = set(
        run("git", "diff", "--name-only", "HEAD~1", cwd=workspace).stdout.splitlines()
    )
    assert {
        "src/config.rs",
        "src/inventory.rs",
        "src/main.rs",
        "src/report.rs",
    } <= changed_at_head
    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""


@pytest.mark.skipif(CARGO is None, reason="Rust toolchain is not installed")
def test_rebuilt_workspace_preserves_rust_task_contract(tmp_path, monkeypatch):
    monkeypatch.setenv("RUSTFLAGS", "-D warnings")
    monkeypatch.setenv("CARGO_ENCODED_RUSTFLAGS", "-D\u001fwarnings")
    workspace = tmp_path / "workspace"
    run(
        sys.executable,
        str(BUILDER),
        "--destination",
        str(workspace),
        cwd=REPO_ROOT,
    )

    tests = run(
        CARGO,
        "test",
        "--quiet",
        "--locked",
        cwd=workspace,
        env=cargo_environment(),
    )
    assert "15 passed" in tests.stdout + tests.stderr
    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""


@pytest.mark.skipif(not HAS_CLIPPY, reason="cargo-clippy is not installed")
def test_rebuilt_workspace_preserves_clippy_task_contract(tmp_path, monkeypatch):
    monkeypatch.setenv("RUSTFLAGS", "-D warnings")
    monkeypatch.setenv("CARGO_ENCODED_RUSTFLAGS", "-D\u001fwarnings")
    workspace = tmp_path / "workspace"
    run(
        sys.executable,
        str(BUILDER),
        "--destination",
        str(workspace),
        cwd=REPO_ROOT,
    )
    clippy_target = tmp_path / "clippy-target"
    clippy = subprocess.run(
        [CARGO, "clippy", "--quiet", "--locked", "--", "-W", "dead_code"],
        cwd=workspace,
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
        env=cargo_environment(CARGO_TARGET_DIR=str(clippy_target)),
    )
    diagnostics = clippy.stdout + clippy.stderr
    assert "apply_discount" in diagnostics
    assert "find_by_sku" in diagnostics
    assert "find_by_category" in diagnostics


def test_rebuild_workspace_force_replaces_mutated_fixture(tmp_path):
    workspace = tmp_path / "workspace"
    base = [
        sys.executable,
        str(BUILDER),
        "--destination",
        str(workspace),
    ]
    run(*base, cwd=REPO_ROOT)
    initial_commit = run("git", "rev-parse", "HEAD", cwd=workspace).stdout
    (workspace / "src" / "config.rs").write_text("broken\n")

    run(*base, "--force", cwd=REPO_ROOT)

    assert "category_labels" in (workspace / "src" / "config.rs").read_text()
    assert run("git", "rev-parse", "HEAD", cwd=workspace).stdout == initial_commit
    assert run("git", "status", "--porcelain", cwd=workspace).stdout == ""


def test_rebuild_workspace_ignores_inherited_git_identity(tmp_path):
    workspace = tmp_path / "workspace"
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "Unexpected Author",
        "GIT_AUTHOR_EMAIL": "unexpected@example.com",
        "GIT_COMMITTER_NAME": "Unexpected Committer",
        "GIT_COMMITTER_EMAIL": "unexpected@example.com",
    }
    subprocess.run(
        [
            sys.executable,
            str(BUILDER),
            "--destination",
            str(workspace),
        ],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
        env=env,
    )
    assert (
        run("git", "rev-parse", "HEAD", cwd=workspace).stdout.strip()
        == EXPECTED_FIXTURE_COMMIT
    )


def test_rebuild_workspace_force_refuses_non_fixture_directory(tmp_path):
    destination = tmp_path / "important"
    destination.mkdir()
    preserved = destination / "keep.txt"
    preserved.write_text("do not delete\n")

    result = subprocess.run(
        [
            sys.executable,
            str(BUILDER),
            "--destination",
            str(destination),
            "--force",
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        timeout=120,
    )

    assert result.returncode != 0
    assert "refusing to replace" in result.stderr
    assert preserved.read_text() == "do not delete\n"
