"""MCP-level tests for the shellcheck tool plugin (#34)."""

import json


def _payload(result):
    return json.loads(result["content"][0]["text"])


# --- Tests ---

def test_shellcheck_in_tool_list(daimonos):
    tools = daimonos.list_tools()
    names = [t["name"] for t in tools]
    assert "shellcheck" in names, f"shellcheck tool not in list; got: {names}"


def test_shellcheck_params_accepted_without_error(daimonos, tmp_path):
    """Verify schema params are forwarded without an RPC-level crash."""
    script = tmp_path / "ok.sh"
    script.write_text("#!/bin/bash\necho hello\n")

    result = daimonos.call_tool("shellcheck", {
        "file": str(script),
        "shell": "bash",
    })
    payload = _payload(result)
    assert "clean" in payload or "error" in payload, f"unexpected payload: {payload}"


def test_shellcheck_clean_script(daimonos, tmp_path):
    script = tmp_path / "ok.sh"
    script.write_text("#!/bin/bash\necho hello\n")

    result = daimonos.call_tool("shellcheck", {"file": str(script)})
    payload = _payload(result)

    assert payload["clean"] is True, f"expected clean=true; got {payload}"
    assert payload["diagnostics"] == [], f"expected empty diagnostics; got {payload}"


def test_shellcheck_returns_diagnostics_for_bad_script(daimonos, tmp_path):
    # SC2086: Double quote to prevent globbing and word splitting
    script = tmp_path / "bad.sh"
    script.write_text("#!/bin/bash\nfoo=$1\necho $foo\n")

    result = daimonos.call_tool("shellcheck", {"file": str(script)})
    payload = _payload(result)

    assert payload["clean"] is False, f"expected clean=false; got {payload}"
    diags = payload["diagnostics"]
    assert len(diags) > 0, "expected at least one diagnostic"

    first = diags[0]
    assert "code" in first, f"missing 'code' field; got {first}"
    assert "message" in first, f"missing 'message' field; got {first}"
    assert "level" in first, f"missing 'level' field; got {first}"
    assert "line" in first, f"missing 'line' field; got {first}"


def test_shellcheck_multiple_files(daimonos, tmp_path):
    s1 = tmp_path / "a.sh"
    s2 = tmp_path / "b.sh"
    s1.write_text("#!/bin/bash\necho hi\n")
    s2.write_text("#!/bin/bash\necho bye\n")

    result = daimonos.call_tool("shellcheck", {
        "files": [str(s1), str(s2)],
    })
    payload = _payload(result)

    assert payload["clean"] is True, f"expected clean=true; got {payload}"


def test_shellcheck_shell_override(daimonos, tmp_path):
    script = tmp_path / "ok.sh"
    script.write_text("#!/bin/sh\necho hi\n")

    result = daimonos.call_tool("shellcheck", {
        "file": str(script),
        "shell": "sh",
    })
    payload = _payload(result)

    assert payload["clean"] is True, f"expected clean=true with sh dialect; got {payload}"


def test_shellcheck_missing_file_returns_error(daimonos):
    result = daimonos.call_tool("shellcheck", {
        "file": "/definitely/nonexistent/script.sh",
    })
    payload = _payload(result)

    assert "error" in payload, (
        f"missing file should produce an error field; got: {payload}"
    )


def test_shellcheck_diagnostic_has_file_path(daimonos, tmp_path):
    """Each diagnostic should reference the file that was checked."""
    script = tmp_path / "bad.sh"
    script.write_text("#!/bin/bash\nfoo=$1\necho $foo\n")

    result = daimonos.call_tool("shellcheck", {"file": str(script)})
    payload = _payload(result)

    assert not payload["clean"]
    diags = payload["diagnostics"]
    assert len(diags) > 0
    # shellcheck JSON includes a "file" key in each diagnostic
    assert "file" in diags[0], f"expected 'file' key in diagnostic; got {diags[0]}"
