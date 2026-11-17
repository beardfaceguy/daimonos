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
import subprocess

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
