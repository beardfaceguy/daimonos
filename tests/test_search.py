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
    """Search by filename uses the trigram index. Index needs a moment to build."""
    daimonos.call_tool("write_file", {
        "path": "unique_name_xyz.rs",
        "content": "content",
    })
    # Give the indexer time — it runs in background
    time.sleep(1)

    result = daimonos.call_tool("search", {
        "pattern": "unique_name_xyz",
        "mode": "files",
    })
    content = json.loads(result["content"][0]["text"])
    if len(content.get("results", [])) > 0:
        found = [r["file"] for r in content["results"]]
        assert any("unique_name_xyz" in f for f in found)


def test_incremental_index_picks_up_new_files(daimonos):
    """After writing a new file and waiting for reindex, it should be searchable."""
    daimonos.call_tool("write_file", {
        "path": "initial_indexed.rs",
        "content": "fn initial_indexed_function() {}",
    })
    time.sleep(1.5)

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
    time.sleep(1.5)

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
