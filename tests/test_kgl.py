"""End-to-end MCP tests for the KGL tools (kgl_query, kgl_assert).

Seeds a tiny x07 workspace via write_file, builds the graph with kgl_query
index, then exercises orientation queries and the assert (write) path over
JSON-RPC against the real binary.
"""

import json
import os

MODULE = json.dumps(
    {
        "module_id": "svc",
        "schema_version": "1.0",
        "kind": "library",
        "imports": ["std.io"],
        "decls": [
            {
                "kind": "defn",
                "name": "authenticate",
                "params": [{"name": "u", "ty": "String"}],
                "result": "Token",
                "effects": ["IO"],
                "body": [{"kind": "call", "callee": "load_user"}],
            },
            {
                "kind": "defn",
                "name": "load_user",
                "params": [{"name": "id", "ty": "String"}],
                "result": "User",
                "effects": ["IO"],
                "body": [{"kind": "call", "callee": "read_file", "args": ["/var/users.db"]}],
            },
        ],
    }
)


def _text(result):
    return json.loads(result["content"][0]["text"])


def _seed(daimonos):
    daimonos.call_tool("write_file", {"path": "svc.x07.json", "content": MODULE})
    idx = _text(daimonos.call_tool("kgl_query", {"query": "index"}))
    assert idx["indexed"] is True
    return idx


def _hash_of(daimonos, name):
    found = _text(daimonos.call_tool("kgl_query", {"query": "find", "args": {"q": name}}))
    return next(n["hash"] for n in found if n["name"] == name)


def test_kgl_query_index_and_find(daimonos):
    idx = _seed(daimonos)
    assert idx["substrate"] == "x07"
    assert idx["nodes"] >= 3  # module + 2 functions

    found = _text(daimonos.call_tool("kgl_query", {"query": "find", "args": {"q": "authenticate"}}))
    assert "authenticate" in [n["name"] for n in found]


def test_kgl_query_neighbors_reads(daimonos):
    _seed(daimonos)
    h = _hash_of(daimonos, "load_user")
    reads = _text(
        daimonos.call_tool("kgl_query", {"query": "neighbors", "args": {"hash": h, "kind": "reads"}})
    )
    assert any(e["to"] == "file:///var/users.db" for e in reads)


def test_kgl_assert_intent_then_visible(daimonos):
    _seed(daimonos)
    h = _hash_of(daimonos, "authenticate")

    out = _text(
        daimonos.call_tool(
            "kgl_assert",
            {
                "action": "intent",
                "args": {"hash": h, "purpose": "verify creds, issue token", "open_questions": ["ttl?"]},
            },
        )
    )
    assert out["updated"] is True

    node = _text(daimonos.call_tool("kgl_query", {"query": "node", "args": {"hash": h}}))
    assert node["intent"]["purpose"] == "verify creds, issue token"

    oq = _text(daimonos.call_tool("kgl_query", {"query": "open_questions"}))
    assert any(n["hash"] == h for n in oq)


def test_kgl_assert_bad_hash_errors(daimonos):
    _seed(daimonos)
    res = daimonos.call_tool("kgl_assert", {"action": "intent", "args": {"hash": "nope", "purpose": "x"}})
    assert res.get("isError") is True


def test_kgl_assert_declare_edge(daimonos):
    _seed(daimonos)
    h = _hash_of(daimonos, "authenticate")
    daimonos.call_tool(
        "kgl_assert",
        {"action": "declare_edge", "args": {"from": h, "to": "secret:DB_PW", "kind": "reads"}},
    )
    nb = _text(
        daimonos.call_tool("kgl_query", {"query": "neighbors", "args": {"hash": h, "kind": "reads"}})
    )
    assert any(e["to"] == "secret:DB_PW" for e in nb)


# graphify node-link graph with a single code node (no *.x07.json present).
GRAPHIFY = json.dumps(
    {
        "nodes": [
            {
                "id": "n_foo",
                "label": ".foo()",
                "file_type": "code",
                "source_file": "src/a.rs",
            }
        ],
        "links": [],
    }
)


def test_kgl_index_without_substrate_detects_graphify(daimonos):
    # Blocker regression at the MCP boundary: `index` with no substrate on a
    # graphify-only workspace must pick graphify, not run an empty x07 scan.
    daimonos.call_tool(
        "write_file", {"path": "graphify-out/graph.json", "content": GRAPHIFY}
    )
    idx = _text(daimonos.call_tool("kgl_query", {"query": "index"}))
    assert idx["indexed"] is True
    assert idx["substrate"] == "graphify"
    assert idx["nodes"] >= 1


def test_kgl_index_unknown_substrate_errors(daimonos):
    res = daimonos.call_tool("kgl_query", {"query": "index", "args": {"substrate": "bogus"}})
    assert res.get("isError") is True


def test_kgl_declare_edge_unknown_source_errors(daimonos):
    # W5: a declared edge from a non-existent node hash must error.
    _seed(daimonos)
    res = daimonos.call_tool(
        "kgl_assert",
        {"action": "declare_edge", "args": {"from": "deadbeef", "to": "file:///x", "kind": "mutates"}},
    )
    assert res.get("isError") is True


def test_kgl_batch_write_is_observed(daimonos_observe):
    # W8 regression: file ops inside a `batch` are observed (the early batch
    # return used to skip the hook). Seed + index first, then write via batch
    # and assert the session shows up as a writer of the touched file.
    daimonos_observe.call_tool("write_file", {"path": "svc.x07.json", "content": MODULE})
    daimonos_observe.call_tool("kgl_query", {"query": "index"})
    daimonos_observe.call_tool(
        "batch",
        {"ops": [{"tool": "write_file", "arguments": {"path": "note.txt", "content": "hi"}}]},
    )
    # observe builds `file://<abs>`; the daemon canonicalizes the workspace, so
    # match it with realpath (≈ Rust std::fs::canonicalize).
    ws_real = os.path.realpath(daimonos_observe.workspace)
    resource = "file://" + ws_real + "/note.txt"
    writers = _text(
        daimonos_observe.call_tool(
            "kgl_query", {"query": "writers_of", "args": {"resource": resource}}
        )
    )
    assert isinstance(writers, list)
    assert len(writers) >= 1


def test_kgl_observe_resolves_relative_path_against_cwd(daimonos_observe):
    # Observe regression: after set_cwd into a subdir, a relative write touches
    # cwd/<path>; the observed edge must be recorded against cwd/<path>, not
    # workspace/<path>, or writers_of returns nothing / points at a ghost file.
    daimonos_observe.call_tool("write_file", {"path": "svc.x07.json", "content": MODULE})
    daimonos_observe.call_tool("kgl_query", {"query": "index"})
    daimonos_observe.call_tool("exec", {"command": "mkdir -p sub"})
    daimonos_observe.call_tool("set_cwd", {"path": "sub"})
    daimonos_observe.call_tool("write_file", {"path": "note.txt", "content": "hi"})

    ws_real = os.path.realpath(daimonos_observe.workspace)
    cwd_resource = "file://" + ws_real + "/sub/note.txt"
    ws_resource = "file://" + ws_real + "/note.txt"
    cwd_writers = _text(
        daimonos_observe.call_tool(
            "kgl_query", {"query": "writers_of", "args": {"resource": cwd_resource}}
        )
    )
    ws_writers = _text(
        daimonos_observe.call_tool(
            "kgl_query", {"query": "writers_of", "args": {"resource": ws_resource}}
        )
    )
    assert len(cwd_writers) >= 1, "edge should be recorded against cwd/sub/note.txt"
    assert ws_writers == [], "edge must NOT be recorded against workspace/note.txt"

# --- context gating (#936 prefix diet): kgl tools hidden unless a store exists ---


def test_kgl_tools_hidden_from_list_without_store(daimonos):
    """Fresh workspace has no .kgl/ — the kgl tools must not spend prefix tokens."""
    names = [t["name"] for t in daimonos.list_tools()]
    assert "kgl_query" not in names, f"kgl_query should be gated; got {names}"
    assert "kgl_assert" not in names, f"kgl_assert should be gated; got {names}"


def test_kgl_tools_listed_once_store_exists(daimonos, tmp_path):
    """context_check is evaluated per list_tools call, so a store created
    mid-session surfaces the tools without a restart."""
    (tmp_path / ".kgl").mkdir()
    names = [t["name"] for t in daimonos.list_tools()]
    assert "kgl_query" in names
    assert "kgl_assert" in names


def test_kgl_tools_still_callable_while_hidden(daimonos):
    """Gating hides the tools from list_tools but must not block dispatch —
    `kgl_query index` is the bootstrap that creates the store."""
    result = daimonos.call_tool("kgl_query", {"query": "index"})
    text = result["content"][0]["text"]
    assert text.strip(), "kgl_query index returned empty response while hidden"
    assert not result.get("isError"), f"kgl_query index failed while hidden: {text}"
