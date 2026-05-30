"""E2e: KGL observed-provenance capture for script-driven file ops.

With DAIMONOS_KGL_OBSERVE=1, an agent running a Starlark script that writes a
file (via the execute_script tool binding -> dispatch_tool_by_name) should leave
an observed `mutates` edge from its session to that file in the KGL graph.
Off by default, nothing is recorded.
"""

import json


def _text(result):
    return json.loads(result["content"][0]["text"])


def test_script_write_is_observed(daimonos_observe):
    d = daimonos_observe
    # An agent composes work as a Starlark script (the incumbent glue).
    d.call_tool("execute_script", {"code": "write_file('out.txt', 'hi')\nresult = 'ok'"})

    # The session's observed mutations are now queryable (no source read).
    nb = _text(
        d.call_tool(
            "kgl_query",
            {"query": "neighbors", "args": {"hash": "session:unknown", "kind": "mutates"}},
        )
    )
    assert any(e["to"].endswith("out.txt") for e in nb), f"expected observed mutates edge, got {nb}"


def test_no_capture_when_disabled(daimonos):
    # Default daimonos fixture has the gate off — nothing recorded.
    d = daimonos
    d.call_tool("execute_script", {"code": "write_file('x.txt', 'y')\nresult = 'ok'"})
    nb = _text(
        d.call_tool(
            "kgl_query",
            {"query": "neighbors", "args": {"hash": "session:unknown", "kind": "mutates"}},
        )
    )
    assert nb == []
