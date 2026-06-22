"""MCP-level tests for the ls tool, including glob and type filters (#36)."""

import json
import os


def _entries(result):
    return json.loads(result["content"][0]["text"])["entries"]


def _names(result):
    return [e["n"] for e in _entries(result)]


def test_ls_lists_files_and_dirs(daimonos):
    daimonos.call_tool("write_file", {"path": "a.txt", "content": "hello"})
    daimonos.call_tool("write_file", {"path": "sub/b.txt", "content": "world"})

    result = daimonos.call_tool("ls", {"depth": 1})
    names = _names(result)
    assert "a.txt" in names
    assert "sub" in names


def test_ls_depth_limits_recursion(daimonos):
    daimonos.call_tool("write_file", {"path": "a/b/deep.txt", "content": "x"})

    shallow = daimonos.call_tool("ls", {"depth": 1})
    deep = daimonos.call_tool("ls", {"depth": 3})

    shallow_names = _names(shallow)
    deep_names = _names(deep)

    assert "a" in shallow_names
    assert "a/b/deep.txt" not in shallow_names, "depth=1 should not reach grandchild"
    assert "a/b/deep.txt" in deep_names, "depth=3 should reach grandchild"


def test_ls_glob_filter_returns_matching_files(daimonos):
    daimonos.call_tool("write_file", {"path": "main.rs", "content": ""})
    daimonos.call_tool("write_file", {"path": "lib.rs", "content": ""})
    daimonos.call_tool("write_file", {"path": "main.py", "content": ""})

    result = daimonos.call_tool("ls", {"glob": "*.rs"})
    names = _names(result)

    assert "main.rs" in names
    assert "lib.rs" in names
    assert "main.py" not in names, "main.py should be excluded by glob=*.rs"


def test_ls_glob_no_match_returns_empty(daimonos):
    daimonos.call_tool("write_file", {"path": "foo.py", "content": ""})

    result = daimonos.call_tool("ls", {"glob": "*.go"})
    names = _names(result)
    assert names == [], f"expected empty, got {names}"


def test_ls_type_filter_files_only(daimonos):
    daimonos.call_tool("write_file", {"path": "file.txt", "content": ""})
    daimonos.call_tool("write_file", {"path": "subdir/nested.txt", "content": ""})

    result = daimonos.call_tool("ls", {"type": "f"})
    entries = _entries(result)

    for e in entries:
        assert not e["d"], f"type=f should exclude dirs but got dir entry: {e}"
    file_names = [e["n"] for e in entries]
    assert "file.txt" in file_names


def test_ls_type_filter_dirs_only(daimonos):
    daimonos.call_tool("write_file", {"path": "file.txt", "content": ""})
    daimonos.call_tool("write_file", {"path": "subdir/nested.txt", "content": ""})

    result = daimonos.call_tool("ls", {"type": "d"})
    entries = _entries(result)

    for e in entries:
        assert e["d"], f"type=d should exclude files but got file entry: {e}"
    dir_names = [e["n"] for e in entries]
    assert "subdir" in dir_names


def test_ls_glob_recursive_finds_nested_matches(daimonos):
    daimonos.call_tool("write_file", {"path": "src/main.rs", "content": ""})
    daimonos.call_tool("write_file", {"path": "src/lib.py", "content": ""})
    daimonos.call_tool("write_file", {"path": "README.md", "content": ""})

    result = daimonos.call_tool("ls", {"depth": 3, "glob": "*.rs"})
    names = _names(result)

    assert "src/main.rs" in names, "should find nested .rs file"
    assert "src/lib.py" not in names, "should not find .py files"
    assert "README.md" not in names, "should not find .md files"
    assert "src" not in names, "src dir doesn't match *.rs"


def test_ls_glob_and_type_combined(daimonos):
    daimonos.call_tool("write_file", {"path": "src/main.rs", "content": ""})
    daimonos.call_tool("write_file", {"path": "src/lib.py", "content": ""})
    daimonos.call_tool("write_file", {"path": "notes.rs", "content": ""})

    result = daimonos.call_tool("ls", {"depth": 3, "glob": "*.rs", "type": "f"})
    names = _names(result)

    assert "src/main.rs" in names
    assert "notes.rs" in names
    assert "src/lib.py" not in names, ".py filtered by glob"
    assert "src" not in names, "src dir filtered by type=f"


def test_ls_skips_build_artifacts(daimonos, tmp_path):
    for skip_dir in ["target", "node_modules", "__pycache__"]:
        os.makedirs(tmp_path / skip_dir, exist_ok=True)
    daimonos.call_tool("write_file", {"path": "src/main.rs", "content": ""})

    result = daimonos.call_tool("ls", {"depth": 2})
    names = _names(result)

    assert "src" in names
    assert "target" not in names, "target should be excluded"
    assert "node_modules" not in names, "node_modules should be excluded"
    assert "__pycache__" not in names, "__pycache__ should be excluded"


def test_ls_tool_in_tool_list(daimonos):
    tools = daimonos.list_tools()
    names = [t["name"] for t in tools]
    assert "ls" in names


def test_ls_glob_and_type_accepted_without_error(daimonos):
    # Verifies the tool accepts glob and type params (schema is terse in list_tools)
    daimonos.call_tool("write_file", {"path": "check.rs", "content": ""})
    result = daimonos.call_tool("ls", {"glob": "*.rs", "type": "f", "depth": 2})
    payload = json.loads(result["content"][0]["text"])
    assert "entries" in payload, f"expected entries key; got {payload}"
