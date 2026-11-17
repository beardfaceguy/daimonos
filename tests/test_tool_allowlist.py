"""FR-02 / WP06: an explicit `--tool-allowlist` is the effective set at every
dispatch boundary.

Context: docs/adr/023-explicit-tool-policy.md (one explicit policy at every
direct and delegated dispatch boundary), reviewer-runtime INTERFACES.md
(`--tool-allowlist PATH`, the `tool_not_allowed` refusal code, the exit-2
`run_configuration_invalid` startup taxonomy) and INVENTORIES.md (WP06 tests
direct, batch, script, pipeline, lazy activation, plugins, and remote routes
against the same effective policy). The run policy lives in `src/run_policy.rs`
(`ToolPolicy`).

These tests drive a real `daimonos` process — the MCP stdio server
(`daimonos_factory`) for listings/dispatch/composite, and a direct startup
subprocess for the startup report and startup refusals. Never private call
sequences; expected values come from the PM inputs and the interface ledger.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

# Reserved spellings (INTERFACES.md / run_audit REFUSAL_CODES).
NOT_ALLOWED = "tool_not_allowed"
REPORT_KEY = "effective_tool_allowlist"
CFG_INVALID = "run_configuration_invalid"


def _write_allowlist(tmp_path: Path, names) -> Path:
    path = tmp_path / "allowlist.json"
    path.write_text(json.dumps(names))
    return path


def _run_startup(daimonos_binary, workspace, args, env=None, timeout=60):
    """Launch `daimonos --mcp -w WS <args>` with a closed stdin so the stdio
    server reaches EOF and exits cleanly after startup, capturing the full
    startup stderr (the effective-list report and any startup-refusal JSON).

    Returns the completed process (returncode + decoded stderr/stdout)."""
    import os

    cmd = [daimonos_binary, "--mcp", "-w", str(workspace), *args]
    return subprocess.run(
        cmd,
        input=b"",
        capture_output=True,
        timeout=timeout,
        env={**os.environ, **env} if env else None,
    )


def _find_json_line(stderr_bytes, predicate):
    """Return the first JSON object line on stderr for which predicate(obj) is
    true, or None. Startup emits one object per line."""
    for raw in stderr_bytes.decode(errors="replace").splitlines():
        raw = raw.strip()
        if not raw:
            continue
        try:
            obj = json.loads(raw)
        except Exception:
            continue
        if isinstance(obj, dict) and predicate(obj):
            return obj
    return None


def _effective_report(stderr_bytes):
    obj = _find_json_line(stderr_bytes, lambda o: REPORT_KEY in o)
    return obj[REPORT_KEY] if obj else None


def _startup_error(stderr_bytes):
    obj = _find_json_line(stderr_bytes, lambda o: "error" in o)
    assert obj is not None, f"no startup error JSON in stderr: {stderr_bytes!r}"
    return obj["error"]


# --- Slice 1: effective-list report (AC-02.6) --------------------------------


def test_effective_list_report_equals_list_passed(daimonos_binary, tmp_path):
    """AC-02.6: at startup Daimonos reports the effective allowlist in a
    machine-readable form, and (with no narrowing legacy restriction) it equals
    the list passed."""
    names = ["read_file", "search", "workspace_info"]
    allowlist = _write_allowlist(tmp_path, names)
    proc = _run_startup(daimonos_binary, tmp_path, ["--tool-allowlist", str(allowlist)])
    report = _effective_report(proc.stderr)
    assert report is not None, (
        f"no {REPORT_KEY} JSON line on stderr: {proc.stderr!r}"
    )
    assert sorted(report) == sorted(names), report


def test_no_allowlist_emits_no_effective_report(daimonos_binary, tmp_path):
    """Default off: with no `--tool-allowlist`, no effective-list report is
    emitted (legacy behavior, nothing to confirm)."""
    proc = _run_startup(daimonos_binary, tmp_path, [])
    assert _effective_report(proc.stderr) is None, proc.stderr


# --- Slice 2: unknown name fails at startup (AC-02.4, requirement 5) ----------


def test_unknown_allowlist_name_fails_at_startup_naming_the_entry(daimonos_binary, tmp_path):
    """AC-02.4 / requirement 5: an allowlist entry that matches no tool makes
    the run fail at startup (exit 2, `run_configuration_invalid`), naming that
    entry; it is never silently ignored."""
    allowlist = _write_allowlist(tmp_path, ["read_file", "no_such_tool", "search"])
    proc = _run_startup(daimonos_binary, tmp_path, ["--tool-allowlist", str(allowlist)])
    assert proc.returncode == 2, (proc.returncode, proc.stderr)
    error = _startup_error(proc.stderr)
    assert error["code"] == CFG_INVALID, error
    assert "no_such_tool" in error["message"], error
    # Fail-closed: no effective-list report was emitted for an invalid selection.
    assert _effective_report(proc.stderr) is None, proc.stderr


def test_known_allowlist_names_start_cleanly(daimonos_binary, tmp_path):
    """A fully valid selection (including composite names) does not fail at
    startup and is reported."""
    names = ["read_file", "search", "batch", "execute_script"]
    allowlist = _write_allowlist(tmp_path, names)
    proc = _run_startup(daimonos_binary, tmp_path, ["--tool-allowlist", str(allowlist)])
    assert proc.returncode == 0, (proc.returncode, proc.stderr)
    assert sorted(_effective_report(proc.stderr) or []) == sorted(names)


# --- Slice 3: dispatch refusal (AC-02.1) + model listing (AC-02.3) -----------

READ_ONLY_SET = ["read_file", "search", "workspace_info"]


def _refusal(result):
    assert result.get("isError"), f"expected a refusal, got: {result}"
    return json.loads(result["content"][0]["text"])


def test_direct_write_call_is_refused_and_nothing_written(daimonos_factory, tmp_path):
    """AC-02.1: with the allowlist set to read-only tools, a direct call to a
    file-writing tool is refused (`tool_not_allowed`) and no file is created."""
    allowlist = _write_allowlist(tmp_path, READ_ONLY_SET)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    target = tmp_path / "new.txt"
    result = client.call_tool("write_file", {"path": "new.txt", "content": "x"})
    payload = _refusal(result)
    assert payload["code"] == NOT_ALLOWED, payload
    assert payload.get("tool") == "write_file", payload
    assert not target.exists(), "a refused write must not create the file"


def test_allowlisted_read_still_works(daimonos_factory, tmp_path):
    """A tool that is on the allowlist dispatches normally."""
    (tmp_path / "file.txt").write_text("hello\nsecond\n")
    allowlist = _write_allowlist(tmp_path, READ_ONLY_SET)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    result = client.call_tool("read_file", {"path": "file.txt"})
    assert not result.get("isError"), result


def test_model_listing_contains_exactly_the_allowlist(daimonos_factory, tmp_path):
    """AC-02.3: the tool listing the model receives contains exactly the
    allowlisted tools (including any otherwise-lazy ones), and nothing else."""
    allowlist = _write_allowlist(tmp_path, READ_ONLY_SET)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    listed = {t["name"] for t in client.list_tools()}
    assert listed == set(READ_ONLY_SET), listed


# --- Slice 4: composite tools (AC-02.2) -------------------------------------
# PM FR-02-1: every inner call of a composite is checked, always, even when the
# composite itself is listed; a listed script/batch must never reach an unlisted
# tool. Composites not on the list are refused outright.

COMPOSITE_SET = ["read_file", "search", "batch", "execute_script"]


def test_batch_inner_write_is_refused_and_nothing_written(daimonos_factory, tmp_path):
    """AC-02.2 (batch): a batch whose child writes is admitted as a batch, but
    the inner write is refused and nothing is written."""
    allowlist = _write_allowlist(tmp_path, COMPOSITE_SET)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    target = tmp_path / "b.txt"
    result = client.call_tool(
        "batch",
        {"ops": [{"tool": "write_file", "arguments": {"path": "b.txt", "content": "x"}}]},
    )
    # The batch itself returns ok; its child carries the refusal.
    text = result["content"][0]["text"]
    assert NOT_ALLOWED in text, text
    assert not target.exists(), "a refused batch child must not create the file"


def test_script_inner_write_is_refused_and_nothing_written(daimonos_factory, tmp_path):
    """AC-02.2 (script): a listed `execute_script` can still only reach allowed
    inner tools — an inner `tool("write_file", ...)` is refused (nested
    arbitrary Starlark cannot bypass the explicit policy) and nothing written."""
    allowlist = _write_allowlist(tmp_path, COMPOSITE_SET)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    target = tmp_path / "s.txt"
    code = 'result = tool("write_file", path="s.txt", content="x")'
    result = client.call_tool("execute_script", {"code": code})
    text = result["content"][0]["text"]
    assert NOT_ALLOWED in text, text
    assert not target.exists(), "a refused script inner call must not write"


def test_script_inner_opcode_write_is_refused(daimonos_factory, tmp_path):
    """The opcode-backed Starlark shortcut (`write_file(...)`) is checked by the
    same policy as the generic `tool(...)` call."""
    allowlist = _write_allowlist(tmp_path, COMPOSITE_SET)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    target = tmp_path / "w.txt"
    code = 'result = write_file("w.txt", "x")'
    result = client.call_tool("execute_script", {"code": code})
    text = result["content"][0]["text"]
    assert NOT_ALLOWED in text, text
    assert not target.exists()


def test_unlisted_execute_script_is_refused_whole(daimonos_factory, tmp_path):
    """A composite not on the list is refused outright, before any Starlark
    runs."""
    allowlist = _write_allowlist(tmp_path, ["read_file", "search"])
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    result = client.call_tool("execute_script", {"code": "result = 1"})
    payload = _refusal(result)
    assert payload["code"] == NOT_ALLOWED, payload
    assert payload.get("tool") == "execute_script", payload


def test_unlisted_batch_is_refused_whole(daimonos_factory, tmp_path):
    """`batch` itself must be on the list; otherwise it is refused outright and
    no child runs."""
    allowlist = _write_allowlist(tmp_path, ["read_file", "search"])
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    result = client.call_tool(
        "batch", {"ops": [{"tool": "read_file", "arguments": {"path": "x"}}]}
    )
    payload = _refusal(result)
    assert payload["code"] == NOT_ALLOWED, payload
    assert payload.get("tool") == "batch", payload


# --- Slice 5: precedence (AC-02.5), lazy listing, opt-out -------------------


def test_lazy_list_all_tools_is_filtered_to_the_allowlist(daimonos_factory, tmp_path):
    """AC-02.3 (lazy/"load more"): `list_all_tools` shows only allowlisted
    tools, so a tiered/lazy listing cannot reveal an unlisted tool."""
    names = ["read_file", "search", "list_all_tools"]
    allowlist = _write_allowlist(tmp_path, names)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    result = client.call_tool("list_all_tools", {})
    listed = {t["name"] for t in json.loads(result["content"][0]["text"])}
    assert listed == set(names), listed


def test_environment_cannot_widen_or_disable_the_allowlist(daimonos_factory, tmp_path):
    """AC-02.5 / requirement 6: nothing the caller did not pass explicitly can
    widen or disable the list. An environment variable that names extra tools is
    ignored: `write_file` stays refused and the model listing is unchanged."""
    names = ["read_file", "search", "workspace_info"]
    allowlist = _write_allowlist(tmp_path, names)
    client = daimonos_factory(
        tmp_path,
        args=["--tool-allowlist", str(allowlist)],
        env={"DAIMONOS_TOOL_ALLOWLIST": "read_file,write_file,exec"},
    )
    result = client.call_tool("write_file", {"path": "n.txt", "content": "x"})
    assert _refusal(result)["code"] == NOT_ALLOWED
    assert not (tmp_path / "n.txt").exists()
    listed = {t["name"] for t in client.list_tools()}
    assert listed == set(names), listed


def test_default_off_is_unchanged_behavior(daimonos_factory, tmp_path):
    """Default off (opt-out regression): with no `--tool-allowlist`, the full
    catalog is listed and a write succeeds, exactly as today."""
    client = daimonos_factory(tmp_path, args=[])
    listed = {t["name"] for t in client.list_tools()}
    assert "write_file" in listed and "batch" in listed, listed
    result = client.call_tool("write_file", {"path": "ok.txt", "content": "hi"})
    assert not result.get("isError"), result
    assert (tmp_path / "ok.txt").read_text() == "hi"


# --- Slice 6: refusal recorded through the WP08 audit (requirement 8) --------
# Reuses the self-contained fake-provider agent harness from test_run_audit.
from test_run_audit import (  # noqa: E402
    DONE,
    _FakeProvider,
    _by_kind,
    _read_events,
    _run,
)


def test_allowlist_refusal_is_recorded_as_tool_refusal(daimonos_binary, tmp_path):
    """FR-02 requirement 8: with `--audit-path` set, an allowlist refusal is
    recorded as a `tool_refusal` whose reason is `tool_not_allowed`, and nothing
    is written. The refusal flows through WP08's boundary observer (its
    `refusal_code` recognizes the reserved `tool_not_allowed` code)."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "seed.txt").write_text("hello\n")
    allowlist = tmp_path / "al.json"
    allowlist.write_text(json.dumps(["read_file"]))
    provider = _FakeProvider([
        ("call", "read_file", {"path": "seed.txt"}),
        ("call", "write_file", {"path": "out.txt", "content": "nope"}),
        ("end", DONE),
    ])
    try:
        completed, audit_path = _run(
            daimonos_binary, tmp_path, provider, "inspect then try to write",
            extra_global=["--tool-allowlist", str(allowlist)],
        )
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stderr
    events = _read_events(audit_path)
    refusals = _by_kind(events, "tool_refusal")
    assert any(e.get("reason") == NOT_ALLOWED for e in refusals), events
    # The allowed read was still recorded as an executed tool_call.
    assert any(e.get("reason") is None for e in _by_kind(events, "tool_call")), events
    assert not (workspace / "out.txt").exists(), "a refused write must not be made"
