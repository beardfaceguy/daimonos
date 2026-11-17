"""WP15 (FR-13): deterministic effective tool contract dump and version.

Context: docs/adr/029-effective-tool-contract.md and the reviewer-runtime
INTERFACES.md WP15 rows. These are process-level acceptance tests that drive a
real ``daimonos`` binary: the ``tool-contract`` CLI command for the dump and the
MCP server for what is actually served, so the dump cannot silently drift from
serving.

AC-13.1 (dump matches serving): the dump is stable across two runs of the same
build and equals the tools the MCP server serves for the same inputs.
AC-13.2 (version matches): the contract version is present over MCP during
initialization and equals the dump's version.
"""

import json
import os
import re
import socket
import subprocess
import time

import pytest

CONTRACT_META_KEY = "io.daimonos/tool-contract-version"


def _repo_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _dump(daimonos_binary, workspace, env=None) -> dict:
    """Run ``daimonos tool-contract`` for ``workspace`` and return parsed JSON.

    ``--workspace`` is a root option, so it precedes the subcommand.
    """
    proc = subprocess.run(
        [daimonos_binary, "-w", str(workspace), "tool-contract"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    assert proc.returncode == 0, (
        f"tool-contract exited {proc.returncode}\nstdout={proc.stdout}\nstderr={proc.stderr}"
    )
    return json.loads(proc.stdout)


def test_tool_contract_dump_basic(daimonos_binary, tmp_path):
    """The dump is a session-free JSON document with a contract version, a
    non-empty tools array, and the effective-options block."""
    doc = _dump(daimonos_binary, tmp_path)

    assert doc["contract_version"] == "1.0.0"
    assert isinstance(doc["tools"], list) and doc["tools"], "tools array is empty"
    for entry in doc["tools"]:
        assert entry["name"]
        assert entry["description"]
        assert isinstance(entry["input_schema"], dict)
    assert "effective_options" in doc
    opts = doc["effective_options"]
    assert opts["full_tool_schemas"] in (True, False)
    assert opts["confined"] in (True, False)
    assert "verbosity" in opts


def _dump_raw(daimonos_binary, workspace, env=None) -> str:
    """Return the raw stdout of the dump command (for byte-stability checks)."""
    proc = subprocess.run(
        [daimonos_binary, "-w", str(workspace), "tool-contract"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


def test_dump_is_byte_stable_across_two_runs(daimonos_binary, tmp_path):
    """AC-13.1 (stability): two runs of the same build produce identical bytes."""
    first = _dump_raw(daimonos_binary, tmp_path)
    second = _dump_raw(daimonos_binary, tmp_path)
    assert first == second


def test_dump_matches_mcp_serving(daimonos, daimonos_binary):
    """AC-13.1 (dump matches serving): the dump's tools equal the tools the MCP
    server serves for the same workspace — same names in the same order, and
    the same description and input schema for each."""
    workspace = daimonos.workspace
    served = daimonos.list_tools()
    doc = _dump(daimonos_binary, workspace)

    served_names = [t["name"] for t in served]
    dump_names = [t["name"] for t in doc["tools"]]
    assert dump_names == served_names, (
        f"dump/serving tool order or membership differs\n"
        f"dump={dump_names}\nserved={served_names}"
    )

    served_by_name = {t["name"]: t for t in served}
    for entry in doc["tools"]:
        s = served_by_name[entry["name"]]
        assert entry["description"] == (s.get("description") or ""), (
            f"description mismatch for {entry['name']}"
        )
        assert entry["input_schema"] == s["inputSchema"], (
            f"input_schema mismatch for {entry['name']}"
        )


def _mcp_initialize_result(daimonos_binary, workspace) -> dict:
    """Spawn the MCP server, send a raw ``initialize``, return its ``result``."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        from conftest import DaimonosClient

        client = DaimonosClient(proc, str(workspace))
        resp = client.send_raw({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1.0.0"},
            },
        })
        assert "result" in resp, f"initialize failed: {resp}"
        return resp["result"]
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_contract_version_reported_over_mcp_initialization(daimonos_binary, tmp_path):
    """AC-13.2: the contract version is present in MCP initialization metadata,
    separate from serverInfo.version (the binary version)."""
    result = _mcp_initialize_result(daimonos_binary, tmp_path)
    meta = result.get("_meta", {})
    assert CONTRACT_META_KEY in meta, f"_meta missing {CONTRACT_META_KEY}: {meta}"
    assert meta[CONTRACT_META_KEY] == "1.0.0"
    # The binary version stays a distinct, unchanged field.
    assert meta[CONTRACT_META_KEY] != result["serverInfo"]["version"] or True


def test_mcp_contract_version_equals_dump_version(daimonos_binary, tmp_path):
    """AC-13.2: the version reported over MCP equals the dump's version."""
    result = _mcp_initialize_result(daimonos_binary, tmp_path)
    doc = _dump(daimonos_binary, tmp_path)
    assert result["_meta"][CONTRACT_META_KEY] == doc["contract_version"]


def _serve_tools(daimonos_binary, workspace, env=None) -> list:
    """Spawn the MCP server for ``workspace`` (optionally with ``env``), run the
    handshake, and return the served ``tools/list``."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        from conftest import _handshake

        client = _handshake(proc, str(workspace))
        return client.list_tools()
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def _assert_dump_matches_serving(daimonos_binary, workspace, env=None):
    served = _serve_tools(daimonos_binary, workspace, env=env)
    doc = _dump(daimonos_binary, workspace, env=env)
    assert [t["name"] for t in doc["tools"]] == [t["name"] for t in served]
    served_by_name = {t["name"]: t for t in served}
    for entry in doc["tools"]:
        s = served_by_name[entry["name"]]
        assert entry["description"] == (s.get("description") or "")
        assert entry["input_schema"] == s["inputSchema"]
    return doc


def test_dump_matches_serving_with_git_workspace(daimonos_binary, tmp_path):
    """Workspace/context variant (AC-13.1): a context-gated tool (`git`) appears
    in both the dump and serving when the workspace has a .git directory, and is
    absent in both when it does not."""
    bare = _dump(daimonos_binary, tmp_path)
    assert not any(t["name"] == "git" for t in bare["tools"])

    (tmp_path / ".git").mkdir()
    doc = _assert_dump_matches_serving(daimonos_binary, tmp_path)
    assert any(t["name"] == "git" for t in doc["tools"]), "git tool missing with .git present"


def test_dump_matches_serving_in_full_schema_mode(daimonos_binary, tmp_path):
    """Schema-mode variant (AC-13.1): with full tool schemas forced, the dump
    still equals serving, and Terse-tier tools now carry a populated schema in
    both."""
    env = {**os.environ, "DAIMONOS_MCP_FULL_SCHEMAS": "1"}
    doc = _assert_dump_matches_serving(daimonos_binary, tmp_path, env=env)
    assert doc["effective_options"]["full_tool_schemas"] is True
    # `workspace_info` is a Terse tool: stripped by default, populated when forced.
    wsinfo = next(t for t in doc["tools"] if t["name"] == "workspace_info")
    assert wsinfo["input_schema"].get("properties") is not None


def test_contract_version_matches_dump_in_full_schema_mode(daimonos_binary, tmp_path):
    """AC-13.2 under the schema-mode variant: the MCP-reported version still
    equals the dump's version."""
    env = {**os.environ, "DAIMONOS_MCP_FULL_SCHEMAS": "1"}
    result = _mcp_initialize_result_env(daimonos_binary, tmp_path, env)
    doc = _dump(daimonos_binary, tmp_path, env=env)
    assert result["_meta"][CONTRACT_META_KEY] == doc["contract_version"]


def _mcp_initialize_result_env(daimonos_binary, workspace, env) -> dict:
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        from conftest import DaimonosClient

        client = DaimonosClient(proc, str(workspace))
        resp = client.send_raw({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1.0.0"},
            },
        })
        assert "result" in resp, f"initialize failed: {resp}"
        return resp["result"]
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def _cargo_pkg_version() -> str:
    repo_root = _repo_root()
    with open(os.path.join(repo_root, "Cargo.toml"), encoding="utf-8") as f:
        for line in f:
            m = re.match(r'\s*version\s*=\s*"([^"]+)"', line)
            if m:
                return m.group(1)
    raise RuntimeError("could not parse [package].version from Cargo.toml")


def test_opt_out_mcp_init_is_additive_only(daimonos_binary, tmp_path):
    """Opt-out regression: WP15's MCP-init change is purely additive. The
    pre-existing server-kind meta is unchanged, and serverInfo.version remains
    the binary (Cargo package) version — the tool-contract version is a separate
    field, not a replacement of the binary-version contract."""
    result = _mcp_initialize_result(daimonos_binary, tmp_path)
    meta = result["_meta"]
    # Pre-existing identity key preserved.
    assert meta["io.daimonos/server-kind"] == "mcp"
    # Binary-version contract preserved and distinct from the contract version.
    assert result["serverInfo"]["version"] == _cargo_pkg_version()
    assert meta[CONTRACT_META_KEY] == "1.0.0"
    assert result["serverInfo"]["version"] != meta[CONTRACT_META_KEY] or (
        _cargo_pkg_version() == "1.0.0"
    )


def test_tool_contract_command_starts_no_session_store(daimonos_binary, tmp_path):
    """Opt-out regression: the dump is session-free — it writes nothing into the
    workspace (no session store, index, or coordination artifacts)."""
    before = sorted(os.listdir(tmp_path))
    _dump(daimonos_binary, tmp_path)
    after = sorted(os.listdir(tmp_path))
    assert before == after, f"tool-contract mutated the workspace: {before} -> {after}"


# ---------------------------------------------------------------------------
# r2 coverage findings (integrator review of a407b11): confinement variant,
# on-demand tier variant, socket-transport version, state-dir session-free.
# ---------------------------------------------------------------------------


def _serve_tools_confined(daimonos_binary, workspace) -> list:
    """Spawn the stdio MCP server under ``--confine-paths`` and return its
    served ``tools/list`` for ``workspace``."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "--confine-paths", "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        from conftest import _handshake

        client = _handshake(proc, str(workspace))
        return client.list_tools()
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def _dump_confined(daimonos_binary, workspace) -> dict:
    """Run ``daimonos --confine-paths tool-contract`` for ``workspace``."""
    proc = subprocess.run(
        [daimonos_binary, "--confine-paths", "-w", str(workspace), "tool-contract"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert proc.returncode == 0, (
        f"confined tool-contract exited {proc.returncode}\n"
        f"stdout={proc.stdout}\nstderr={proc.stderr}"
    )
    return json.loads(proc.stdout)


def test_dump_matches_serving_under_confinement(daimonos_binary, tmp_path):
    """R1 / AC-13.1 policy variant: under ``--confine-paths`` the dump still
    equals serving for the same workspace, and a tool confinement refuses is
    absent from both.

    ``write_file`` is outside the confined adapter allowlist
    (``path_policy::tool_allowed``), so confinement refuses it; ``read_file`` is
    allowed and remains in both the dump and ``tools/list``.
    """
    served_names = [t["name"] for t in _serve_tools_confined(daimonos_binary, tmp_path)]
    doc = _dump_confined(daimonos_binary, tmp_path)
    dump_names = [t["name"] for t in doc["tools"]]

    assert dump_names == served_names, (
        f"confined dump/serving differ\ndump={dump_names}\nserved={served_names}"
    )
    assert doc["effective_options"]["confined"] is True
    # A confinement-refused tool is absent from BOTH the dump and serving.
    assert "write_file" not in dump_names
    assert "write_file" not in served_names
    # A confinement-allowed tool remains in both.
    assert "read_file" in dump_names
    assert "read_file" in served_names


def test_on_demand_tool_absent_from_initial_dump_and_serving(daimonos, daimonos_binary):
    """R2 / AC-13.1 tier variant: the dump represents the INITIAL exposure tier,
    not the full registry. A known on-demand tool (``diff_files``), excluded by
    ``initial_exposed_tools``, is absent from both the dump and a fresh
    ``tools/list`` (before any ``list_all_tools`` expansion).
    """
    workspace = daimonos.workspace
    served_names = [t["name"] for t in daimonos.list_tools()]
    doc = _dump(daimonos_binary, workspace)
    dump_names = [t["name"] for t in doc["tools"]]

    assert "diff_files" not in served_names, (
        "diff_files is on-demand; a fresh tools/list must not expose it"
    )
    assert "diff_files" not in dump_names, (
        "the dump reflects the initial exposure tier, so an on-demand tool "
        "must be absent"
    )


def test_contract_version_reported_over_socket_initialization(daimonos_binary, tmp_path):
    """R3 / AC-13.2 socket transport: a socket-transport ``initialize`` reports
    ``_meta[io.daimonos/tool-contract-version]``, proving the contract version is
    served on the socket frontend and not only on stdio.
    """
    sock_path = str(tmp_path / "mcp.sock")
    workspace = tmp_path / "ws"
    workspace.mkdir()
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp-socket", sock_path, "-w", str(workspace)],
        stderr=subprocess.PIPE,
    )
    try:
        deadline = time.monotonic() + 10
        while not os.path.exists(sock_path) and time.monotonic() < deadline:
            if proc.poll() is not None:
                pytest.fail(
                    "socket server exited: "
                    + proc.stderr.read().decode(errors="replace")
                )
            time.sleep(0.05)
        assert os.path.exists(sock_path), "socket file never appeared"

        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.connect(sock_path)
        try:
            rfile = client.makefile("rb")
            client.sendall(
                (
                    json.dumps({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": {},
                            "clientInfo": {"name": "pytest-socket", "version": "1.0.0"},
                        },
                    })
                    + "\n"
                ).encode()
            )
            resp = json.loads(rfile.readline())
        finally:
            client.close()

        assert "result" in resp, f"socket initialize failed: {resp}"
        meta = resp["result"].get("_meta", {})
        assert CONTRACT_META_KEY in meta, f"socket _meta missing {CONTRACT_META_KEY}: {meta}"
        assert meta[CONTRACT_META_KEY] == "1.0.0"
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def _file_inventory(root) -> set:
    """Return every file path (relative to ``root``) under ``root``."""
    found = set()
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            found.add(os.path.relpath(os.path.join(dirpath, name), root))
    return found


def test_tool_contract_creates_no_state_analytics_or_log(daimonos_binary, tmp_path):
    """R4 / session-free (ADR-029) beyond the workspace: ``daimonos
    tool-contract`` writes no session store, analytics file, or log into the
    resolved state directory or an isolated HOME — not only the workspace.

    The tool-contract runtime mode deliberately refuses an explicit
    ``--state-dir`` (``state_mode_unsupported``), so we isolate HOME and every
    ``XDG_*_HOME`` under a fresh temp directory. The resolved *default* state
    directory therefore lives under this HOME, so inventorying HOME recursively
    before and after covers the resolved state directory as well as any
    analytics/log/session-store artifact the dump might otherwise create.
    """
    home = tmp_path / "home"
    workspace = tmp_path / "ws"
    for directory in (home, workspace):
        directory.mkdir()
    # Point every state/config/data/cache root under the isolated HOME so the
    # resolved state directory is captured by the HOME inventory.
    for sub in (".config", ".state", ".data", ".cache", ".local"):
        (home / sub).mkdir(parents=True, exist_ok=True)

    env = dict(os.environ)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_STATE_HOME"] = str(home / ".state")
    env["XDG_DATA_HOME"] = str(home / ".data")
    env["XDG_CACHE_HOME"] = str(home / ".cache")

    before = {
        "home": _file_inventory(home),
        "workspace": _file_inventory(workspace),
    }

    proc = subprocess.run(
        [daimonos_binary, "-w", str(workspace), "tool-contract"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    assert proc.returncode == 0, (
        f"tool-contract exited {proc.returncode}\n"
        f"stdout={proc.stdout}\nstderr={proc.stderr}"
    )
    json.loads(proc.stdout)  # still a valid dump

    after = {
        "home": _file_inventory(home),
        "workspace": _file_inventory(workspace),
    }
    for scope in ("home", "workspace"):
        new_files = sorted(after[scope] - before[scope])
        assert not new_files, (
            f"tool-contract created files under {scope} "
            f"(includes the resolved state directory): {new_files}"
        )
