"""Tests for MCP initialize handshake and tool listing."""

import subprocess


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
        assert result["serverInfo"]["version"] == "0.1.0"
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
        assert "Workspace:" in instructions
        assert str(tmp_path) in instructions
        assert "list_all_tools" in instructions
        assert "IMPORTANT" in instructions
        assert "read_file" in instructions
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_list_tools_returns_all_tools(daimonos):
    """All tools are exposed from the start for Cursor compatibility."""
    tools = daimonos.list_tools()
    tool_names = {t["name"] for t in tools}

    core = {"read_file", "write_file", "edit_file", "search",
            "workspace_info", "exec", "batch", "list_all_tools"}
    extended = {"snapshot_create", "snapshot_restore", "snapshot_list",
                "snapshot_delete", "diff_files", "git_status", "git_log",
                "git_diff", "git_branch", "tool_pipeline", "tool_repair"}

    assert core.issubset(tool_names)
    assert extended.issubset(tool_names)


def test_list_all_tools_returns_catalog(daimonos):
    """list_all_tools returns a catalog of all available tools."""
    import json

    result = daimonos.call_tool("list_all_tools", {})
    catalog = json.loads(result["content"][0]["text"])
    catalog_names = {t["name"] for t in catalog}

    assert "git_status" in catalog_names
    assert "snapshot_create" in catalog_names
    assert "diff_files" in catalog_names
    assert "batch" in catalog_names


def test_list_tools_have_input_schema(daimonos):
    """Verify all tools have proper input schemas."""
    import json

    # First unlock all tools
    daimonos.call_tool("list_all_tools", {})

    tools = daimonos.list_tools()
    for tool in tools:
        assert "inputSchema" in tool, f"{tool['name']} missing inputSchema"
        assert tool["inputSchema"]["type"] == "object"
