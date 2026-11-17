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


def test_workspace_config_cannot_widen_or_disable_the_allowlist(daimonos_factory, tmp_path):
    """AC-02.5 / requirement 6 (R5): the REAL precedence vectors — a workspace
    `daimonos.toml` that (a) declares an extra local tool and (b) tries to set
    `tool_allowlist` itself — cannot widen or disable the caller's explicit
    `--tool-allowlist`. `cfg.tool_allowlist` is `#[serde(skip)]`, so the config
    key is ignored, and a config-registered tool is still subject to the CLI
    list. The tool the config named (write_file) and the extra tool stay
    refused; the model listing is exactly the CLI list."""
    # (a) registers a local tool `extra`; (b) attempts to set the allowlist.
    (tmp_path / "daimonos.toml").write_text(
        'tool_allowlist = ["write_file", "exec"]\n'
        '[tools.extra]\nbin = "/bin/true"\n')
    names = ["read_file", "search"]
    allowlist = _write_allowlist(tmp_path, names)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    # The config's attempt to set the allowlist is ignored: a tool it named
    # (write_file) stays refused.
    result = client.call_tool("write_file", {"path": "n.txt", "content": "x"})
    assert _refusal(result)["code"] == NOT_ALLOWED
    assert not (tmp_path / "n.txt").exists()
    # The config-registered `extra` tool is not on the CLI allowlist -> refused.
    refused_extra = client.call_tool("extra", {})
    assert _refusal(refused_extra)["code"] == NOT_ALLOWED
    # The listing is exactly the CLI list (config neither widened nor disabled).
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


# =====================================================================
# Run r2 review fixes (ADR-023, PM Q-FR-02-2). Each test drives a real
# daimonos process. Reserved reasons (INTERFACES.md exit-2 taxonomy):
MISSING_FINAL = "tool_allowlist_missing_final_output"
# Reuse the hermetic control-file + isolated-env helpers from the WP05 suite.
from test_structured_output import (  # noqa: E402
    SCHEMA,
    _isolated_env,
    _write_control,
    _last_json,
)


# --- R1 (BLOCKER): omitted final-answer tool fails startup -------------------


def test_final_output_tool_absent_from_allowlist_fails_startup(daimonos_binary, tmp_path):
    """R1 / PM Q-FR-02-2 / ADR-023: when `--tool-allowlist` is active and
    `--final-output` is configured, an allowlist that omits the final-answer
    tool fails the run at startup (exit 2, `run_configuration_invalid`,
    reason `tool_allowlist_missing_final_output`) naming the omitted tool. The
    name is never added automatically."""
    home = tmp_path / "home"
    home.mkdir()
    control = _write_control(
        tmp_path, schema=SCHEMA, tool_name="final_answer",
        tool_description="finish", corrective_retries=0)
    allowlist = _write_allowlist(tmp_path, ["read_file", "search"])
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path),
         "--tool-allowlist", str(allowlist), "agent",
         "--final-output", control, "--print", "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    assert completed.returncode == 2, (completed.returncode, completed.stderr)
    error = _startup_error(completed.stderr.encode())
    assert error["code"] == CFG_INVALID, error
    assert error["reason"] == MISSING_FINAL, error
    assert "final_answer" in error["message"], error


def test_final_output_tool_on_allowlist_starts_cleanly(daimonos_binary, tmp_path):
    """The complement: when the final-answer tool IS on the allowlist, the
    missing-final-output guard does not fire (startup proceeds past it). The
    effective-list report is emitted and includes the final-answer name."""
    home = tmp_path / "home"
    home.mkdir()
    control = _write_control(
        tmp_path, schema=SCHEMA, tool_name="final_answer",
        tool_description="finish", corrective_retries=0)
    allowlist = _write_allowlist(tmp_path, ["read_file", "search", "final_answer"])
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path),
         "--tool-allowlist", str(allowlist), "agent",
         "--final-output", control, "--print", "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    # It must not fail with the missing-final-output reason (any later failure
    # for want of a provider/credential is fine — the startup guard passed).
    report = _effective_report(completed.stderr.encode())
    assert report is not None and "final_answer" in report, completed.stderr
    missing = _find_json_line(
        completed.stderr.encode(),
        lambda o: o.get("error", {}).get("reason") == MISSING_FINAL,
    )
    assert missing is None, completed.stderr


# --- Listing-inventory helpers ----------------------------------------------


def _sig_tool_names(text):
    """Extract the set of daimonos tool names a `list_tool_signatures` /
    server-instructions signature dump advertises. `print`/`tool` are language
    builtins (not daimonos tools) and are not counted."""
    names = set()
    marker = 'Callable as tool("<name>", ...): '
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("def "):
            nm = line[4:].split("(", 1)[0]
            if nm not in ("print", "tool"):
                names.add(nm)
        if marker in line:
            for n in line.split(marker, 1)[1].split(","):
                n = n.strip()
                if n:
                    names.add(n)
    return names


def _server_instructions(daimonos_binary, workspace, args):
    """Return the MCP server `instructions` string from a fresh initialize."""
    import os
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(workspace), *args],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=os.environ.copy(),
    )
    try:
        req = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                          "clientInfo": {"name": "pytest", "version": "1.0"}}}
        proc.stdin.write((json.dumps(req) + "\n").encode())
        proc.stdin.flush()
        line = proc.stdout.readline()
        obj = json.loads(line)
        return obj.get("result", {}).get("instructions", "") or ""
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()


# --- R2 (MAJOR): get_tool_schema is filtered by the policy ------------------


def test_get_tool_schema_of_unlisted_tool_returns_nothing(daimonos_factory, tmp_path):
    """R2 / ADR-023: under an active allowlist, `get_tool_schema` must not
    reveal the schema of an unlisted tool — the probe set equals the allowlist.
    A listed tool's schema is returned; an unlisted name yields an unknown-tool
    error with no schema, and the "Available" suggestion omits unlisted tools."""
    names = ["read_file", "search", "get_tool_schema"]
    allowlist = _write_allowlist(tmp_path, names)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    ok = client.call_tool("get_tool_schema", {"tools": ["read_file"]})
    assert not ok.get("isError"), ok
    assert "read_file" in ok["content"][0]["text"], ok
    res = client.call_tool("get_tool_schema", {"tools": ["write_file"]})
    text = res["content"][0]["text"]
    assert res.get("isError"), text
    assert "inputSchema" not in text, text  # no schema for an unlisted tool
    available = text.split("Available:", 1)[1] if "Available:" in text else ""
    assert "write_file" not in available, text


# --- R3 (MAJOR): list_tool_signatures is filtered by the policy -------------


def test_list_tool_signatures_is_filtered_to_the_allowlist(daimonos_factory, tmp_path):
    """R3 / ADR-023: `list_tool_signatures` dumps only signatures of allowlisted
    tools. The advertised set is a subset of the allowlist and never reveals an
    unlisted tool (served, discovered, probed, and dumped sets must agree)."""
    names = ["read_file", "search", "exec", "list_tool_signatures"]
    allowlist = _write_allowlist(tmp_path, names)
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    res = client.call_tool("list_tool_signatures", {})
    text = res["content"][0]["text"]
    parsed = _sig_tool_names(text)
    assert parsed <= set(names), (parsed, names)
    assert {"read_file", "search", "exec"} <= parsed, (parsed, text)
    assert "write_file" not in parsed and "ls" not in parsed, parsed


def test_server_instructions_signatures_are_filtered(daimonos_binary, tmp_path):
    """Listing inventory: the MCP server `instructions` embed Starlark tool
    signatures. Under an active allowlist they are filtered too, so the
    instructions cannot reveal an unlisted tool (ADR-023 served==discovered)."""
    names = ["read_file", "search", "execute_script"]
    allowlist = _write_allowlist(tmp_path, names)
    instr = _server_instructions(daimonos_binary, tmp_path, ["--tool-allowlist", str(allowlist)])
    assert "def read_file(" in instr, instr
    assert "def write_file(" not in instr, instr
    assert "def exec(" not in instr, instr


# --- R4 (MINOR): effective report intersects the legacy restriction ---------


def test_effective_report_intersects_legacy_restriction(daimonos_binary, tmp_path):
    """R4 / ADR-023: the startup effective-list report is the intersection with
    the run's existing legacy restriction (agent-env `allowed_commands`), not
    the raw list — it reports the set the run will actually permit."""
    home = tmp_path / "home"
    home.mkdir()
    env_file = tmp_path / "agent.env"
    env_file.write_text("DAIMONOS_AGENT_ALLOWED_COMMANDS=read_file\n")
    allowlist = _write_allowlist(tmp_path, ["read_file", "search"])
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(tmp_path),
         "--tool-allowlist", str(allowlist), "agent",
         "--agent-env", str(env_file), "--print", "hi"],
        capture_output=True, text=True, timeout=90, env=_isolated_env(home))
    report = _effective_report(completed.stderr.encode())
    assert report == ["read_file"], (report, completed.stderr)


# --- R7 (MINOR): config-registered custom tools are allowlistable -----------


def test_config_registered_tool_can_be_allowlisted(daimonos_binary, tmp_path):
    """R7 / ADR-023 effective catalog: a config-registered custom local tool is
    a dispatchable name, so allowlisting it must not false-reject at startup."""
    (tmp_path / "daimonos.toml").write_text('[tools.mytool]\nbin = "/bin/true"\n')
    allowlist = _write_allowlist(tmp_path, ["read_file", "mytool"])
    proc = _run_startup(daimonos_binary, tmp_path, ["--tool-allowlist", str(allowlist)])
    assert proc.returncode == 0, (proc.returncode, proc.stderr)
    report = _effective_report(proc.stderr)
    assert report is not None and "mytool" in report, proc.stderr


# =====================================================================
# Run r3 review fixes (second independent review, commit 5ef5e05).
# R1 (MAJOR): the model-facing catalog on the chat/ACP/interactive/session
#   frontends was NOT filtered by an active `--tool-allowlist` (only
#   `agent --print` filtered). The fix is a single source filter in
#   `tool_facade::active_schemas_for_policy`, so every frontend offers only
#   allowlisted tools. These tests drive a real frontend process against a
#   local fake provider that RECORDS the `tools` array of the model request and
#   asserts it equals the allowlist (ADR-023 served==dispatched).
# R2 (MAJOR): the `execute_script` tool DESCRIPTION embedded the full,
#   unfiltered Starlark signature dump on every agent-family frontend
#   (including `agent --print`). Under an active allowlist the block must list
#   only allowlisted Starlark tools.
import os as _os_r3
import threading as _threading_r3
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer  # noqa: E402

_R3_DONE = "R3_ALLOWLIST_AGENT_DONE"


class _RecordingProvider:
    """Local fake OpenAI-compatible (`openrouter`) endpoint that RECORDS the
    `tools` offered in each model request, then finishes the turn immediately
    with an assistant stop message. No network, no real credential."""

    def __init__(self):
        self.requested_tools = []  # per-POST list of OpenAI tool dicts
        self.post_count = 0
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{"id": "test/model",
                    "context_length": 100000, "supported_parameters": ["tools"],
                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                owner.post_count += 1
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                owner.requested_tools.append(body.get("tools") or [])
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                frame = {"choices": [{"index": 0,
                    "delta": {"role": "assistant", "content": _R3_DONE},
                    "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}}
                try:
                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = _threading_r3.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self):
        return self.server.server_port

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

    def offered_tool_names(self, which=0):
        assert self.requested_tools, "no model request was recorded"
        return {t["function"]["name"] for t in self.requested_tools[which]}

    def offered_description(self, name, which=0):
        assert self.requested_tools, "no model request was recorded"
        for t in self.requested_tools[which]:
            if t["function"]["name"] == name:
                return t["function"].get("description", "")
        return None


def _r3_env(tmp_path):
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    (tmp_path / "tmp").mkdir(exist_ok=True)
    env = {k: v for k, v in _os_r3.environ.items() if not k.startswith("DAIMONOS_")}
    env.update({
        "HOME": str(home),
        "XDG_STATE_HOME": str(home / "state"),
        "XDG_DATA_HOME": str(home / "data"),
        "XDG_CONFIG_HOME": str(home / "config"),
        "TMPDIR": str(tmp_path / "tmp"),
        "DAIMONOS_KGL_AUTOINDEX": "0",
    })
    return env


def _r3_agent_env(tmp_path, provider, allowed):
    """Agent env pointing at the recording provider. `allowed` is the legacy
    agent-env allowed_commands (dispatch restriction); it does not shape the
    offered catalog, which is filtered by the explicit --tool-allowlist."""
    path = tmp_path / "rec-agent.env"
    path.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.port}/v1",
        "DAIMONOS_AGENT_API_KEY=synthetic-not-a-real-key-R3",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        f"DAIMONOS_AGENT_ALLOWED_COMMANDS={allowed}",
        "",
    ]))
    return path


R3_LIST = ["read_file", "search", "ls"]


def _drive_acp_prompt(daimonos_binary, tmp_path, provider, allowlist, allowed):
    """Drive a real `daimonos acp` process through initialize -> session/new ->
    session/prompt so it builds its model-facing catalog and issues one model
    request, which the recording provider captures. ACP is JSON-RPC over stdio
    (deterministic, no pty)."""
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir(exist_ok=True)
    agent_env = _r3_agent_env(tmp_path, provider, allowed)
    env = _r3_env(tmp_path)
    command = [daimonos_binary, "--workspace", str(workspace),
               "--tool-allowlist", str(allowlist),
               "acp", "--agent-env", str(agent_env)]
    process = subprocess.Popen(
        command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, env=env,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        init = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {},
                       "clientInfo": {"name": "pytest", "version": "1"}}})
        assert "result" in init, init
        created = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": str(workspace), "mcpServers": []}})
        assert "sessionId" in created.get("result", {}), created
        sid = created["result"]["sessionId"]
        client.send_raw({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": sid,
                       "prompt": [{"type": "text", "text": "hi"}]}})
    finally:
        if process.poll() is None:
            _terminate(process)
    return process


def test_acp_offers_only_the_allowlist(daimonos_binary, tmp_path):
    """R1 (MAJOR) / AC-02.3: with `--tool-allowlist` active, the ACP frontend
    offers the model EXACTLY the allowlisted tools (recorded model request),
    not the full catalog. Before the single-source fix the ACP catalog was
    unfiltered."""
    allowlist = _write_allowlist(tmp_path, R3_LIST)
    provider = _RecordingProvider()
    try:
        _drive_acp_prompt(daimonos_binary, tmp_path, provider, allowlist,
                          allowed="read_file,search,ls")
    finally:
        provider.close()
    assert provider.post_count >= 1, "ACP issued no model request"
    assert provider.offered_tool_names() == set(R3_LIST), provider.offered_tool_names()


def test_agent_print_offers_only_the_allowlist(daimonos_binary, tmp_path):
    """R1 regression: `agent --print` (dbunk's frontend) must keep offering the
    model exactly the allowlist after the per-frontend retain is removed in
    favor of the single source filter."""
    allowlist = _write_allowlist(tmp_path, R3_LIST)
    provider = _RecordingProvider()
    try:
        workspace = tmp_path / "workspace"
        workspace.mkdir()
        agent_env = _r3_agent_env(tmp_path, provider, allowed="read_file,search,ls")
        env = _r3_env(tmp_path)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--tool-allowlist", str(allowlist),
               "agent", "--print", "--agent-env", str(agent_env), "hi"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=90, env=env)
    finally:
        provider.close()
    assert completed.returncode == 0, completed.stderr
    assert provider.post_count >= 1, completed.stderr
    assert provider.offered_tool_names() == set(R3_LIST), provider.offered_tool_names()


def test_execute_script_description_lists_only_allowlisted_signatures(daimonos_binary, tmp_path):
    """R2 (MAJOR) / ADR-023: under an active allowlist, the `execute_script`
    tool DESCRIPTION's embedded Starlark signature block must list only
    allowlisted tools. With `execute_script` and `read_file` on the list, the
    recorded `execute_script` description contains `def read_file(` but not
    `def write_file(` (an unlisted tool)."""
    names = ["read_file", "search", "execute_script"]
    allowlist = _write_allowlist(tmp_path, names)
    provider = _RecordingProvider()
    try:
        _drive_acp_prompt(daimonos_binary, tmp_path, provider, allowlist,
                          allowed="read_file,search")
    finally:
        provider.close()
    assert provider.post_count >= 1, "ACP issued no model request"
    desc = provider.offered_description("execute_script")
    assert desc is not None, provider.offered_tool_names()
    assert "def read_file(" in desc, desc
    assert "def write_file(" not in desc, desc
    assert "def exec(" not in desc, desc


# =====================================================================
# Run r4 review fixes (third independent review, commit c53af27).
# R1 (MAJOR / ADR-023, PM Q-FR-02-1): the composite opcodes `tool_pipeline`
# and `lint_repair` delegate to an inner registered tool named by `tool_id`.
# A listed composite must never reach an UNLISTED inner tool — the inner name
# is checked against the explicit policy before the registry runs it, and the
# inner tool never runs. These tests drive a real MCP process with a
# config-registered `[tools.marker]` tool (NOT on the allowlist) whose command
# would create a marker file if it ran. R4 (MINOR): a malformed or unreadable
# `--tool-allowlist` file fails startup with the exit-2 reason
# `tool_allowlist_invalid` (INTERFACES.md taxonomy).
import os as _os_r4  # noqa: E402

INVALID = "tool_allowlist_invalid"  # reserved exit-2 reason (INTERFACES.md)


def _write_marker_tool(tmp_path, marker):
    """Write an executable that creates `marker` when run; return its path.

    The `[tools.*]` config registers a generic plugin with a single `run`
    command whose bin is this script (args are empty), so the marker path is
    baked into the script body.
    """
    script = tmp_path / "marker_tool.sh"
    script.write_text("#!/bin/sh\necho ran > '%s'\n" % marker)
    script.chmod(0o755)
    return script


def test_tool_pipeline_inner_tool_not_on_allowlist_is_refused(daimonos_factory, tmp_path):
    """R1 / ADR-023 / PM Q-FR-02-1: with `tool_pipeline` on the allowlist and a
    config-registered `[tools.marker]` tool that is NOT, `tool_pipeline` naming
    that inner `tool_id` is refused (`tool_not_allowed`, naming the inner tool)
    and the inner tool never runs (no marker file). Delegation cannot bypass the
    explicit policy."""
    marker = tmp_path / "pipeline_ran.marker"
    tool = _write_marker_tool(tmp_path, marker)
    (tmp_path / "daimonos.toml").write_text('[tools.marker]\nbin = "%s"\n' % tool)
    allowlist = _write_allowlist(tmp_path, ["read_file", "tool_pipeline"])
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    result = client.call_tool("tool_pipeline", {"tool_id": "marker", "stages": ["run"]})
    payload = _refusal(result)
    assert payload["code"] == NOT_ALLOWED, payload
    assert payload.get("tool") == "marker", payload
    assert not marker.exists(), "a refused pipeline inner tool must not run"


def test_lint_repair_inner_tool_not_on_allowlist_is_refused(daimonos_factory, tmp_path):
    """R1 / ADR-023 / PM Q-FR-02-1: with `lint_repair` on the allowlist and a
    config-registered `[tools.marker]` tool that is NOT, `lint_repair` naming
    that inner `tool_id` is refused (`tool_not_allowed`) BEFORE the registry is
    consulted — the response is the policy refusal, not an inner
    `unknown command 'lint'` error, and the inner tool never runs."""
    marker = tmp_path / "repair_ran.marker"
    tool = _write_marker_tool(tmp_path, marker)
    (tmp_path / "daimonos.toml").write_text('[tools.marker]\nbin = "%s"\n' % tool)
    allowlist = _write_allowlist(tmp_path, ["read_file", "lint_repair"])
    client = daimonos_factory(tmp_path, args=["--tool-allowlist", str(allowlist)])
    result = client.call_tool("lint_repair", {"tool_id": "marker"})
    assert result.get("isError"), result
    text = result["content"][0]["text"]
    assert NOT_ALLOWED in text, text  # not an inner "unknown command" error
    payload = json.loads(text)
    assert payload["code"] == NOT_ALLOWED, payload
    assert payload.get("tool") == "marker", payload
    assert "unknown command" not in text, text  # delegation never reached
    assert not marker.exists(), "a refused lint_repair inner tool must not run"


# --- R4 (MINOR): malformed / unreadable allowlist file fails startup --------


def test_malformed_allowlist_file_fails_at_startup(daimonos_binary, tmp_path):
    """R4 / INTERFACES.md exit-2 taxonomy: a `--tool-allowlist` file that is not
    valid JSON fails the run at startup (exit 2, `run_configuration_invalid`,
    reason `tool_allowlist_invalid`); no effective-list report is emitted."""
    path = tmp_path / "bad.json"
    path.write_text("{ not valid json ]")
    proc = _run_startup(daimonos_binary, tmp_path, ["--tool-allowlist", str(path)])
    assert proc.returncode == 2, (proc.returncode, proc.stderr)
    error = _startup_error(proc.stderr)
    assert error["code"] == CFG_INVALID, error
    assert error["reason"] == INVALID, error
    assert _effective_report(proc.stderr) is None, proc.stderr


def test_unreadable_allowlist_file_fails_at_startup(daimonos_binary, tmp_path):
    """R4: an unreadable `--tool-allowlist` file fails startup with the same
    `tool_allowlist_invalid` reason (the read error is reported, not silently
    treated as legacy/no-list)."""
    if hasattr(_os_r4, "geteuid") and _os_r4.geteuid() == 0:
        pytest.skip("root bypasses file permissions; cannot exercise unreadable path")
    path = tmp_path / "noperm.json"
    path.write_text('["read_file"]')
    path.chmod(0o000)
    try:
        proc = _run_startup(daimonos_binary, tmp_path, ["--tool-allowlist", str(path)])
    finally:
        path.chmod(0o644)
    assert proc.returncode == 2, (proc.returncode, proc.stderr)
    error = _startup_error(proc.stderr)
    assert error["code"] == CFG_INVALID, error
    assert error["reason"] == INVALID, error
