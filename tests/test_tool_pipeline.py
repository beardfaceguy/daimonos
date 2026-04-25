"""Integration tests for tool_pipeline and tool_repair MCP tools.

These tools operate on the ToolRegistry -- they require pre-registered external
tool plugins. Since there is no MCP-level tool_register endpoint, these tests
exercise the error paths and verify correct MCP routing. The happy paths are
covered by Rust unit tests in tool_runner.rs and ops/tool_ops.rs.
"""

import json
import pytest


class TestToolPipeline:
    """Tests for the tool_pipeline MCP tool."""

    def test_tool_pipeline_visible_in_list(self, daimonos):
        tools = daimonos.list_tools()
        names = [t["name"] for t in tools]
        assert "tool_pipeline" in names

    def test_tool_pipeline_missing_tool_id(self, daimonos):
        result = daimonos.call_tool("tool_pipeline", {
            "stages": ["build", "test"],
        })
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()

    def test_tool_pipeline_missing_stages(self, daimonos):
        result = daimonos.call_tool("tool_pipeline", {
            "tool_id": "nonexistent",
        })
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()

    def test_tool_pipeline_unknown_tool(self, daimonos):
        result = daimonos.call_tool("tool_pipeline", {
            "tool_id": "does_not_exist",
            "stages": ["build"],
        })
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()

    def test_tool_pipeline_empty_stages(self, daimonos):
        result = daimonos.call_tool("tool_pipeline", {
            "tool_id": "some_tool",
            "stages": [],
        })
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()


class TestToolRepair:
    """Tests for the tool_repair MCP tool."""

    def test_tool_repair_visible_in_list(self, daimonos):
        tools = daimonos.list_tools()
        names = [t["name"] for t in tools]
        assert "tool_repair" in names

    def test_tool_repair_missing_tool_id(self, daimonos):
        result = daimonos.call_tool("tool_repair", {})
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()

    def test_tool_repair_unknown_tool(self, daimonos):
        result = daimonos.call_tool("tool_repair", {
            "tool_id": "nonexistent_tool",
        })
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()

    def test_tool_repair_with_max_iterations(self, daimonos):
        result = daimonos.call_tool("tool_repair", {
            "tool_id": "nonexistent_tool",
            "max_iterations": 1,
        })
        content = result["content"][0]["text"]
        assert result.get("isError") is True or "error" in content.lower()
