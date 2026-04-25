"""Tests for snapshot_create, snapshot_restore, snapshot_list, snapshot_delete MCP tools."""

import json
import os


def _parse(result):
    text = result["content"][0]["text"]
    return json.loads(text)


def test_snapshot_create(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original content")

    data = _parse(daimonos.call_tool("snapshot_create", {"tag": "v1"}))
    assert data["tag"] == "v1"
    assert data["file_count"] >= 1
    assert data["total_bytes"] > 0
    assert len(data["id"]) > 0


def test_snapshot_create_without_tag(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content")

    data = _parse(daimonos.call_tool("snapshot_create"))
    assert data["tag"] is None
    assert data["file_count"] >= 1


def test_snapshot_restore_roundtrip(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("original")

    snap = _parse(daimonos.call_tool("snapshot_create", {"tag": "before-edit"}))

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("modified!!!")
    with open(os.path.join(ws, "extra.txt"), "w") as f:
        f.write("new file")

    assert os.path.exists(os.path.join(ws, "extra.txt"))

    restored = _parse(daimonos.call_tool("snapshot_restore", {"id": snap["id"]}))
    assert restored["id"] == snap["id"]

    with open(os.path.join(ws, "file.txt")) as f:
        assert f.read() == "original"
    assert not os.path.exists(os.path.join(ws, "extra.txt"))


def test_snapshot_restore_nonexistent(daimonos):
    result = daimonos.call_tool("snapshot_restore", {"id": "nonexistent-id"})
    assert result.get("isError") is True


def test_snapshot_list_empty(daimonos):
    data = _parse(daimonos.call_tool("snapshot_list"))
    assert data["snapshots"] == []


def test_snapshot_list_with_entries(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content")

    daimonos.call_tool("snapshot_create", {"tag": "first"})
    daimonos.call_tool("snapshot_create", {"tag": "second"})

    data = _parse(daimonos.call_tool("snapshot_list"))
    assert len(data["snapshots"]) == 2
    tags = [s["tag"] for s in data["snapshots"]]
    assert "first" in tags
    assert "second" in tags


def test_snapshot_delete(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("content")

    snap = _parse(daimonos.call_tool("snapshot_create", {"tag": "temp"}))

    data = _parse(daimonos.call_tool("snapshot_list"))
    assert len(data["snapshots"]) == 1

    delete_result = _parse(daimonos.call_tool("snapshot_delete", {"id": snap["id"]}))
    assert delete_result["deleted"] == snap["id"]

    data = _parse(daimonos.call_tool("snapshot_list"))
    assert len(data["snapshots"]) == 0


def test_snapshot_delete_nonexistent(daimonos):
    result = daimonos.call_tool("snapshot_delete", {"id": "nope"})
    assert result.get("isError") is True


def test_snapshot_preserves_nested_structure(daimonos):
    ws = daimonos.workspace
    os.makedirs(os.path.join(ws, "src", "nested"), exist_ok=True)
    with open(os.path.join(ws, "src/nested/deep.rs"), "w") as f:
        f.write("fn deep() {}")
    with open(os.path.join(ws, "root.txt"), "w") as f:
        f.write("root")

    snap = _parse(daimonos.call_tool("snapshot_create", {"tag": "nested"}))
    assert snap["file_count"] == 2

    os.remove(os.path.join(ws, "src/nested/deep.rs"))
    assert not os.path.exists(os.path.join(ws, "src/nested/deep.rs"))

    daimonos.call_tool("snapshot_restore", {"id": snap["id"]})
    assert os.path.exists(os.path.join(ws, "src/nested/deep.rs"))
    with open(os.path.join(ws, "src/nested/deep.rs")) as f:
        assert f.read() == "fn deep() {}"


def test_multiple_snapshots_independent(daimonos):
    ws = daimonos.workspace
    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("v1")

    snap_v1 = _parse(daimonos.call_tool("snapshot_create", {"tag": "v1"}))

    with open(os.path.join(ws, "file.txt"), "w") as f:
        f.write("v2")

    snap_v2 = _parse(daimonos.call_tool("snapshot_create", {"tag": "v2"}))

    daimonos.call_tool("snapshot_restore", {"id": snap_v1["id"]})
    with open(os.path.join(ws, "file.txt")) as f:
        assert f.read() == "v1"

    daimonos.call_tool("snapshot_restore", {"id": snap_v2["id"]})
    with open(os.path.join(ws, "file.txt")) as f:
        assert f.read() == "v2"


def test_tools_list_includes_snapshot_tools(daimonos):
    """Snapshot tools are visible in the initial tool listing."""
    tools = daimonos.list_tools()
    tool_names = [t["name"] for t in tools]
    assert "snapshot_create" in tool_names
    assert "snapshot_restore" in tool_names
    assert "snapshot_list" in tool_names
    assert "snapshot_delete" in tool_names
