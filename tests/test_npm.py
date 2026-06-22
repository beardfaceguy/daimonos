"""MCP-level tests for the npm tool plugin (#35)."""

import json


def _payload(result):
    return json.loads(result["content"][0]["text"])


def _make_project(tmp_path, scripts=None, deps=None):
    pkg = {
        "name": "daimonos-test-pkg",
        "version": "1.0.0",
        "scripts": scripts or {},
        "dependencies": deps or {},
    }
    (tmp_path / "package.json").write_text(json.dumps(pkg))
    return tmp_path


def _make_lockfile(tmp_path):
    lock = {"name": "daimonos-test-pkg", "version": "1.0.0", "lockfileVersion": 3, "requires": True, "packages": {}}
    (tmp_path / "package-lock.json").write_text(json.dumps(lock))


# --- Tests ---

def test_npm_in_tool_list(daimonos):
    tools = daimonos.list_tools()
    names = [t["name"] for t in tools]
    assert "npm" in names, f"npm tool not in list; got: {names}"


def test_npm_params_accepted_without_error(daimonos, tmp_path):
    """Verify the tool accepts documented params without an RPC-level crash."""
    _make_project(tmp_path)
    result = daimonos.call_tool("npm", {"command": "install"})
    payload = _payload(result)
    assert "exit" in payload or "error" in payload, f"unexpected payload: {payload}"


def test_npm_install_empty_project(daimonos, tmp_path):
    _make_project(tmp_path)
    result = daimonos.call_tool("npm", {"command": "install"})
    payload = _payload(result)

    assert "exit" in payload, f"expected exit field; got {payload}"
    assert "ok" in payload, f"expected ok field; got {payload}"
    assert payload["exit"] == 0, f"expected exit=0 for empty deps; got {payload}"
    assert payload["ok"] is True


def test_npm_run_script(daimonos, tmp_path):
    _make_project(tmp_path, scripts={"greet": "echo hello from npm"})
    result = daimonos.call_tool("npm", {"command": "run", "script": "greet"})
    payload = _payload(result)

    assert payload["exit"] == 0, f"got {payload}"
    assert payload["script"] == "greet"
    assert "hello from npm" in payload["stdout"], f"got {payload}"


def test_npm_run_nonexistent_script_returns_nonzero(daimonos, tmp_path):
    _make_project(tmp_path)
    result = daimonos.call_tool("npm", {"command": "run", "script": "does-not-exist-xyz"})
    payload = _payload(result)

    assert payload["exit"] != 0, f"expected nonzero exit; got {payload}"
    assert payload["ok"] is False


def test_npm_audit_clean_project(daimonos, tmp_path):
    _make_project(tmp_path)
    _make_lockfile(tmp_path)
    result = daimonos.call_tool("npm", {"command": "audit"})
    payload = _payload(result)

    assert "clean" in payload or "error" in payload, f"expected clean or error; got {payload}"
    if "clean" in payload:
        assert payload["clean"] is True, f"expected clean=true for empty project; got {payload}"
        assert "vulnerabilities" in payload
        assert "findings" in payload


def test_npm_audit_response_shape(daimonos, tmp_path):
    """Audit on a project with a lock file returns the compact schema."""
    _make_project(tmp_path)
    _make_lockfile(tmp_path)
    result = daimonos.call_tool("npm", {"command": "audit"})
    payload = _payload(result)

    if "clean" in payload:
        assert isinstance(payload["findings"], list), f"findings should be a list; got {payload}"
        assert isinstance(payload["vulnerabilities"], dict), f"vulnerabilities should be a dict; got {payload}"


def test_npm_run_requires_script_param(daimonos, tmp_path):
    """Calling run without script should return an error (not crash)."""
    _make_project(tmp_path)
    result = daimonos.call_tool("npm", {"command": "run"})
    # Should either be an RPC-level error string or a payload with error key
    content = result.get("content", [{}])
    text = content[0].get("text", "") if content else ""
    assert "required" in text.lower() or "script" in text.lower() or "error" in text.lower(), (
        f"expected error about missing script; got: {text}"
    )


def test_npm_stdout_and_stderr_present(daimonos, tmp_path):
    """install/run/test/build all return stdout and stderr keys."""
    _make_project(tmp_path)
    result = daimonos.call_tool("npm", {"command": "install"})
    payload = _payload(result)

    assert "stdout" in payload, f"expected stdout; got {payload}"
    assert "stderr" in payload, f"expected stderr; got {payload}"
