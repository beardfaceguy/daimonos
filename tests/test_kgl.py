"""End-to-end MCP tests for the KGL tools (kgl_query, kgl_assert).

Seeds a tiny x07 workspace via write_file, builds the graph with kgl_query
index, then exercises orientation queries and the assert (write) path over
JSON-RPC against the real binary.
"""

import json

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
