"""Tests for the batch MCP tool."""
import json



def test_batch_read_multiple_files(daimonos, tmp_path):
    """Batch reading multiple files returns all results."""
    (tmp_path / "a.txt").write_text("alpha")
    (tmp_path / "b.txt").write_text("beta")
    (tmp_path / "c.txt").write_text("gamma")

    result = daimonos.call_tool("batch", {
        "ops": [
            {"tool": "read_file", "arguments": {"path": "a.txt"}},
            {"tool": "read_file", "arguments": {"path": "b.txt"}},
            {"tool": "read_file", "arguments": {"path": "c.txt"}},
        ]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 3
    assert all(r["ok"] for r in content)
    assert "alpha" in json.dumps(content[0]["data"])
    assert "beta" in json.dumps(content[1]["data"])
    assert "gamma" in json.dumps(content[2]["data"])


def test_batch_mixed_tools(daimonos, tmp_path):
    """Batch can mix different tool types."""
    (tmp_path / "hello.txt").write_text("hello world")

    result = daimonos.call_tool("batch", {
        "ops": [
            {"tool": "read_file", "arguments": {"path": "hello.txt"}},
            {"tool": "search", "arguments": {"pattern": "hello", "glob": "*.txt"}},
            {"tool": "workspace_info"},
        ]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 3
    assert content[0]["ok"]
    assert content[0]["tool"] == "read_file"
    assert content[1]["ok"]
    assert content[1]["tool"] == "search"
    assert content[2]["ok"]
    assert content[2]["tool"] == "workspace_info"


def test_batch_partial_failure(daimonos, tmp_path):
    """Batch continues on failure and reports per-op status."""
    (tmp_path / "exists.txt").write_text("yes")

    result = daimonos.call_tool("batch", {
        "ops": [
            {"tool": "read_file", "arguments": {"path": "exists.txt"}},
            {"tool": "read_file", "arguments": {"path": "nonexistent.txt"}},
            {"tool": "read_file", "arguments": {"path": "exists.txt"}},
        ]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 3
    assert content[0]["ok"]
    assert not content[1]["ok"]
    assert content[2]["ok"]


def test_batch_empty_ops(daimonos):
    """Batch with empty ops array returns empty results."""
    result = daimonos.call_tool("batch", {"ops": []})
    content = json.loads(result["content"][0]["text"])
    assert content == []


def test_batch_missing_tool_field(daimonos):
    """Batch ops without 'tool' field produce errors."""
    result = daimonos.call_tool("batch", {
        "ops": [{"arguments": {"path": "a.txt"}}]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 1
    assert not content[0]["ok"]
    assert "missing 'tool' field" in content[0]["error"]


def test_batch_unknown_tool(daimonos):
    """Batch with unknown tool name reports error for that op."""
    result = daimonos.call_tool("batch", {
        "ops": [{"tool": "nonexistent_tool", "arguments": {}}]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 1
    assert not content[0]["ok"]


def test_batch_write_then_read(daimonos, tmp_path):
    """Batch can write a file then read it back (sequential execution)."""
    result = daimonos.call_tool("batch", {
        "ops": [
            {"tool": "write_file", "arguments": {"path": "new.txt", "content": "created via batch"}},
            {"tool": "read_file", "arguments": {"path": "new.txt"}},
        ]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 2
    assert content[0]["ok"]
    assert content[1]["ok"]
    assert "created via batch" in json.dumps(content[1]["data"])


def test_batch_nested_not_allowed(daimonos):
    """Nested batch calls are rejected."""
    result = daimonos.call_tool("batch", {
        "ops": [{"tool": "batch", "arguments": {"ops": []}}]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 1
    assert not content[0]["ok"]
    assert "nested batch" in content[0]["error"]


def test_batch_no_ops_field(daimonos):
    """Batch without ops field returns error."""
    result = daimonos.call_tool("batch", {})
    assert result.get("isError") is True


def test_batch_exec(daimonos, tmp_path):
    """Batch can run exec commands."""
    result = daimonos.call_tool("batch", {
        "ops": [
            {"tool": "exec", "arguments": {"command": "echo", "args": ["hello"]}},
            {"tool": "exec", "arguments": {"command": "echo", "args": ["world"]}},
        ]
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content) == 2
    assert content[0]["ok"]
    assert content[1]["ok"]


def test_execute_script_large_result_is_bounded(daimonos):
    result = daimonos.call_tool("execute_script", {
        "code": 'result = "x" * 60000',
    })
    visible = result["content"][0]["text"]
    assert len(visible.encode()) <= 51_200
    assert "full output saved to" in visible or "full_output_path" in visible


def test_batch_final_aggregate_is_bounded(daimonos, tmp_path):
    for name in ["large-a.txt", "large-b.txt", "large-c.txt"]:
        (tmp_path / name).write_text(name + "\n" + ("x" * 20_000))

    result = daimonos.call_tool("batch", {
        "ops": [
            {"tool": "read_file", "arguments": {"path": "large-a.txt"}},
            {"tool": "read_file", "arguments": {"path": "large-b.txt"}},
            {"tool": "read_file", "arguments": {"path": "large-c.txt"}},
        ]
    })
    visible = result["content"][0]["text"]
    assert len(visible.encode()) <= 51_200
    assert "full output saved to" in visible or "full_output_path" in visible
