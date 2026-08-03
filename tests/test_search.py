"""Tests for search MCP tool (content grep and file name search)."""

import json
import time


def test_grep_finds_content(daimonos):
    daimonos.call_tool("write_file", {
        "path": "search1.txt",
        "content": "hello world\nfoo bar\nhello again",
    })
    daimonos.call_tool("write_file", {
        "path": "search2.txt",
        "content": "no match here",
    })

    result = daimonos.call_tool("search", {
        "pattern": "hello",
        "mode": "content",
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content["matches"]) >= 2
    files = {m["f"] for m in content["matches"]}
    assert "search1.txt" in files


def test_grep_with_glob_filter(daimonos):
    daimonos.call_tool("write_file", {
        "path": "a.rs",
        "content": "fn main() {}",
    })
    daimonos.call_tool("write_file", {
        "path": "b.txt",
        "content": "fn main() {}",
    })

    result = daimonos.call_tool("search", {
        "pattern": "fn main",
        "mode": "content",
        "glob": "*.rs",
    })
    content = json.loads(result["content"][0]["text"])
    files = {m["f"] for m in content["matches"]}
    assert "a.rs" in files
    assert "b.txt" not in files


def test_grep_max_results(daimonos):
    for i in range(5):
        daimonos.call_tool("write_file", {
            "path": f"many_{i}.txt",
            "content": "findme\n" * 3,
        })

    result = daimonos.call_tool("search", {
        "pattern": "findme",
        "mode": "content",
        "max_results": 3,
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content["matches"]) <= 3


def test_file_search_via_trigram(daimonos):
    """Successful writes make filenames searchable immediately."""
    daimonos.call_tool("write_file", {
        "path": "unique_name_xyz.rs",
        "content": "content",
    })
    result = daimonos.call_tool("search", {
        "pattern": "unique_name_xyz",
        "mode": "files",
    })
    content = json.loads(result["content"][0]["text"])
    found = [r["file"] for r in content["results"]]
    assert any("unique_name_xyz" in f for f in found)
    assert content["index"]["mode"] == "hybrid"
    assert content["index"]["coverage"] in {"cold", "complete", "partial"}


def test_file_search_honors_path_scope_and_filename_glob(daimonos):
    daimonos.call_tool(
        "write_file", {"path": "root_match.rs", "content": "content"}
    )
    daimonos.call_tool(
        "write_file", {"path": "src/scoped_match.rs", "content": "content"}
    )
    daimonos.call_tool(
        "write_file", {"path": "src/scoped_match.txt", "content": "content"}
    )

    result = daimonos.call_tool(
        "search",
        {
            "pattern": "match",
            "mode": "files",
            "path": "src",
            "glob": "*.rs",
        },
    )
    content = json.loads(result["content"][0]["text"])
    assert [match["file"] for match in content["results"]] == [
        "src/scoped_match.rs"
    ]


def test_file_search_rejects_invalid_glob(daimonos):
    result = daimonos.call_tool(
        "search", {"pattern": "match", "mode": "files", "glob": "["}
    )
    assert result["isError"] is True
    assert "invalid glob" in result["content"][0]["text"]


def test_incremental_index_picks_up_new_files(daimonos):
    """Content grep remains complete independently from the path index."""
    daimonos.call_tool("write_file", {
        "path": "initial_indexed.rs",
        "content": "fn initial_indexed_function() {}",
    })
    result = daimonos.call_tool("search", {
        "pattern": "initial_indexed_function",
        "mode": "content",
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content["matches"]) >= 1

    daimonos.call_tool("write_file", {
        "path": "later_added.rs",
        "content": "fn later_added_unique_func() {}",
    })
    result = daimonos.call_tool("search", {
        "pattern": "later_added_unique_func",
        "mode": "content",
    })
    content = json.loads(result["content"][0]["text"])
    assert len(content["matches"]) >= 1


def test_index_stats_in_workspace_info(daimonos):
    """workspace_info should report index stats including file count."""
    daimonos.call_tool("write_file", {
        "path": "indexed_file.txt",
        "content": "some content for index",
    })
    time.sleep(1)

    result = daimonos.call_tool("workspace_info")
    content = json.loads(result["content"][0]["text"])
    index = content.get("index", {})
    assert "files" in index
    assert "trigrams" in index
    assert index["mode"] == "hybrid"
    assert index["coverage"] in {"cold", "building", "complete", "partial"}
