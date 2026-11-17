"""Startup plugin probes must not install a Rust toolchain.

`cargo` is a rustup shim. When HOME has no `.rustup` and the working directory
sits inside a project with a pinned `rust-toolchain.toml`, a bare `cargo
--version` makes the shim download and install the pinned toolchain (~615 MB,
network). Daimonos probes `cargo --version` at startup to decide whether to
register the cargo tool, so the probe must set `RUSTUP_AUTO_INSTALL=0` for the
child, turning "toolchain not installed" into a clean unavailable instead of a
download.

This is a process-level test: it runs the real daimonos binary with a stub
`cargo` first on PATH that records its environment, then asserts the stub saw
`RUSTUP_AUTO_INSTALL=0` — proving the *product* set it.
"""

from __future__ import annotations

import os
import stat
import time


def test_cargo_probe_sets_rustup_auto_install_zero(
    daimonos_factory, tmp_path, monkeypatch
):
    # The PRODUCT, not this test's environment, must set the variable. Remove it
    # from what the child inherits so the assertion can only pass if the cargo
    # probe sets it itself. (conftest sets it via setdefault for the rest of the
    # suite; here we deliberately strip it.)
    monkeypatch.delenv("RUSTUP_AUTO_INSTALL", raising=False)

    # Isolated HOME: no ~/.rustup and no ~/.cargo/bin, so startup PATH
    # enhancement can't shadow our stub with a real cargo.
    fake_home = tmp_path / "home"
    fake_home.mkdir()

    # Stub `cargo` placed first on PATH: it dumps its environment to a file so we
    # can inspect what the probe handed it.
    stub_dir = tmp_path / "bin"
    stub_dir.mkdir()
    env_dump = tmp_path / "cargo_probe_env.txt"
    stub = stub_dir / "cargo"
    stub.write_text(
        "#!/bin/sh\n"
        f"env > '{env_dump}'\n"
        "echo 'cargo 1.0.0 (daimonos test stub)'\n"
    )
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    workspace = tmp_path / "ws"
    workspace.mkdir()

    child_env = {
        "HOME": str(fake_home),
        "PATH": f"{stub_dir}{os.pathsep}{os.environ.get('PATH', '')}",
    }

    client = daimonos_factory(workspace, env=child_env)
    # Force registration of availability-gated plugins (the cargo probe runs here
    # if it has not already at startup).
    client.list_tools()

    deadline = time.time() + 5.0
    while not env_dump.exists() and time.time() < deadline:
        time.sleep(0.05)

    assert env_dump.exists(), "cargo stub was never invoked by the startup probe"

    recorded: dict[str, str] = {}
    for line in env_dump.read_text().splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            recorded[key] = value

    assert recorded.get("RUSTUP_AUTO_INSTALL") == "0", (
        "startup cargo probe must set RUSTUP_AUTO_INSTALL=0 so a rustup shim "
        "reports the toolchain as unavailable instead of installing it; stub saw "
        f"{recorded.get('RUSTUP_AUTO_INSTALL')!r}"
    )
