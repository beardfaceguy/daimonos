"""Tests for MCP initialize handshake and tool listing."""

import os
import re
import subprocess


def _expected_version() -> str:
    """Read the daimonos package version from `Cargo.toml` so this test
    moves in lockstep with the binary instead of pinning a string. Both
    the MCP `serverInfo.version` and the opcode-schema `version` field
    are now wired to `env!("CARGO_PKG_VERSION")` at compile time, so
    asserting against `Cargo.toml` catches any drift in either layer."""
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with open(os.path.join(repo_root, "Cargo.toml"), encoding="utf-8") as f:
        for line in f:
            m = re.match(r'\s*version\s*=\s*"([^"]+)"', line)
            if m:
                return m.group(1)
    raise RuntimeError("could not parse [package].version from Cargo.toml")


def test_initialize_returns_server_info(daimonos_binary, tmp_path):
    """Raw handshake test — verify server info without the fixture's auto-handshake."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(tmp_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        from conftest import DaimonosClient

        client = DaimonosClient(proc, str(tmp_path))
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

        assert "result" in resp
        result = resp["result"]
        assert result["serverInfo"]["name"] == "daimonos"
        assert result["serverInfo"]["version"] == _expected_version()
        assert "protocolVersion" in result
        assert "capabilities" in result
        assert "tools" in result["capabilities"]
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_initialize_instructions_contain_workspace_context(daimonos_binary, tmp_path):
    """Instructions should contain proactive workspace context."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(tmp_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        from conftest import DaimonosClient

        client = DaimonosClient(proc, str(tmp_path))
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

        instructions = resp["result"].get("instructions", "")
        assert "daimonos" in instructions.lower()
        assert str(tmp_path) in instructions
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def _instructions(daimonos_binary, workspace) -> str:
    """Spawn the MCP server and return the handshake `instructions` string."""
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
        return resp["result"].get("instructions", "")
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_initialize_instructions_contain_terse_directive(daimonos_binary, tmp_path):
    """The externalized default MCP instructions (prompts/mcp_instructions.md)
    are still served over the handshake — regression for vikunja #974."""
    instructions = _instructions(daimonos_binary, tmp_path)
    assert "Terse output" in instructions
    assert "Use daimonos tools" in instructions


def test_prompts_override_replaces_mcp_instructions(daimonos_binary, tmp_path):
    """A `[prompts].mcp_instructions` override in daimonos.toml replaces the
    embedded default at runtime (vikunja #974, Option A) — proves prompts are
    editable without recompiling."""
    custom = tmp_path / "custom_mcp.md"
    custom.write_text("SENTINEL custom mcp instructions line.\n", encoding="utf-8")
    (tmp_path / "daimonos.toml").write_text(
        "[prompts]\nmcp_instructions = " + repr(str(custom)) + "\n",
        encoding="utf-8",
    )
    instructions = _instructions(daimonos_binary, tmp_path)
    assert "SENTINEL custom mcp instructions line." in instructions
    # The embedded default's terse directive must be gone (fully replaced).
    assert "Terse output" not in instructions
    # Dynamic context is still appended after the override.
    assert str(tmp_path) in instructions


def test_tool_description_override_reaches_list_tools(daimonos_binary, tmp_path):
    """A partial top-level description catalog overrides one tool while
    preserving embedded defaults for all others (vikunja #975)."""
    custom = tmp_path / "tool_descriptions.toml"
    custom.write_text(
        '[read_file]\nfull = "SENTINEL custom read description."\n',
        encoding="utf-8",
    )
    (tmp_path / "daimonos.toml").write_text(
        "[prompts]\ntool_descriptions = " + repr(str(custom)) + "\n",
        encoding="utf-8",
    )
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(tmp_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        from conftest import DaimonosClient

        client = DaimonosClient(proc, str(tmp_path))
        init = client.send_raw({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1.0.0"},
            },
        })
        assert "result" in init
        client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})
        tools = {tool["name"]: tool for tool in client.list_tools()}
        assert tools["read_file"]["description"] == "SENTINEL custom read description."
        assert tools["write_file"]["description"] == "Write file, creating parent dirs."
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_list_tools_returns_core_tools(daimonos):
    """Core tools + git + snapshots are exposed by default."""
    tools = daimonos.list_tools()
    tool_names = {t["name"] for t in tools}

    core = {"read_file", "write_file", "edit_file", "search",
            "workspace_info", "exec", "batch", "list_all_tools",
            "snapshot", "set_cwd"}

    assert core.issubset(tool_names)

    hidden = {"diff_files", "tool_pipeline", "tool_repair"}
    for name in hidden:
        assert name not in tool_names, f"{name} should be hidden by default"


def test_list_all_tools_returns_catalog(daimonos):
    """list_all_tools returns a catalog of all available tools."""
    import json

    result = daimonos.call_tool("list_all_tools", {})
    catalog = json.loads(result["content"][0]["text"])
    catalog_names = {t["name"] for t in catalog}

    assert "git" in catalog_names
    assert "snapshot" in catalog_names
    assert "diff_files" in catalog_names
    assert "batch" in catalog_names


def test_list_tools_have_input_schema(daimonos):
    """Verify all tools have proper input schemas."""

    # First unlock all tools
    daimonos.call_tool("list_all_tools", {})

    tools = daimonos.list_tools()
    for tool in tools:
        assert "inputSchema" in tool, f"{tool['name']} missing inputSchema"
        assert tool["inputSchema"]["type"] == "object"
