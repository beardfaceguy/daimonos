"""WP17 (FR-15): self-contained full-schema serving and export.

Context: docs/adr/030-self-contained-full-schemas.md and the reviewer-runtime
INTERFACES.md WP17 rows. These are process-level acceptance tests driving a real
``daimonos`` binary through the ``tool-contract`` dump (FR-13, WP15) and the MCP
server, the surfaces named by AC-15.1.

dbunk always enables full-schema mode (PM CP-FR-15-1). The mode is opt-in: the
legacy terse/lazy placeholder serving stays the default when it is off.

AC-15.1 (metaschema): every schema in the dump validates against the JSON Schema
draft 2020-12 metaschema, and contains no ``$ref`` to anything outside itself.

The 2020-12 metaschema validation is performed authoritatively by the daimonos
binary itself: with the mode on, the ``tool-contract`` dump runs the ADR-024
Draft-2020-12 validator over every served schema and refuses the run with
``schema_unusable`` if any schema is not valid self-contained 2020-12 (ADR-030).
A zero exit therefore proves every schema validated against the metaschema. The
"no external ``$ref``" half is additionally checked here, independently, by
walking the exported JSON (no third-party validator needed, and none is
installed in this environment).
"""

import json
import os
import subprocess

import pytest


def _dump(daimonos_binary, workspace, *extra_args, env=None):
    """Run ``daimonos [extra_args] -w WS tool-contract`` and return (proc, doc).

    ``doc`` is the parsed JSON when the command succeeds, else ``None``.
    Global flags precede the subcommand.
    """
    proc = subprocess.run(
        [daimonos_binary, *extra_args, "-w", str(workspace), "tool-contract"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    doc = json.loads(proc.stdout) if proc.returncode == 0 and proc.stdout else None
    return proc, doc


def _external_refs(schema) -> list:
    """Return every ``$ref`` in ``schema`` that points outside the document.

    A self-contained 2020-12 schema only uses local JSON-pointer/anchor
    references, which begin with ``#``. Anything else (another file, a URL, a
    bare name) is an external reference and fails AC-15.1.
    """
    found = []

    def walk(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key == "$ref" and isinstance(value, str) and not value.startswith("#"):
                    found.append(value)
                walk(value)
        elif isinstance(node, list):
            for item in node:
                walk(item)

    walk(schema)
    return found


def test_cli_full_tool_schemas_flag_populates_the_dump(daimonos_binary, tmp_path):
    """Slice 01: the global ``--full-tool-schemas`` flag turns on full-schema
    serving for the ``tool-contract`` dump, equivalent to ``[mcp]
    full_tool_schemas = true`` — without touching the environment.

    With the flag the effective-options block reports the mode on, no tool is
    served the stripped ``{"type": "object"}`` placeholder, and no schema
    carries an external ``$ref`` (AC-15.1, external-reference half)."""
    proc, doc = _dump(daimonos_binary, tmp_path, "--full-tool-schemas")
    assert proc.returncode == 0, (
        f"tool-contract --full-tool-schemas exited {proc.returncode}\n"
        f"stdout={proc.stdout}\nstderr={proc.stderr}"
    )
    assert doc["effective_options"]["full_tool_schemas"] is True

    stripped = [t["name"] for t in doc["tools"] if t["input_schema"] == {"type": "object"}]
    assert not stripped, f"full-schema mode still served stripped placeholders: {stripped}"

    for tool in doc["tools"]:
        external = _external_refs(tool["input_schema"])
        assert not external, f"{tool['name']} has external $ref(s): {external}"


# ---------------------------------------------------------------------------
# AC-15.1 and the amended owned/supplied-schema cases (WP17 checkpoint).
# ---------------------------------------------------------------------------

# Formerly-stripped tools: with the mode OFF the dump serves these the stripped
# ``{"type": "object"}`` placeholder (progressive exposure); with the mode ON
# they carry their full schema. Stated here independently of the implementation
# (integrator baseline measurement); the test only checks the subset that is
# always exposed in a bare workspace, so it never depends on context-gated or
# on-demand tools being present.
_ALWAYS_EXPOSED_TERSE_TOOLS = [
    "batch",
    "list_all_tools",
    "ls",
    "session_stats",
    "set_cwd",
    "set_external_session_id",
    "set_verbosity",
    "snapshot",
    "workspace_info",
]

# The reviewed owned-path inventory, stated independently of
# ``src/path_annotations.rs::path_arguments`` (the WP16 method, see
# tests/test_path_annotations.py): tool -> {property: kind}. ``scalar`` marks the
# property node; ``items`` marks the array's ``items`` node (path elements).
# Any tool not listed owns no path argument and must carry no mark at all.
_PATH_INVENTORY = {
    "read_file": {"path": "scalar"},
    "write_file": {"path": "scalar"},
    "append_file": {"path": "scalar"},
    "edit_file": {"path": "scalar"},
    "pytest": {"path": "scalar"},
    "set_cwd": {"path": "scalar"},
    "search": {"path": "scalar", "glob": "scalar"},
    "ls": {"path": "scalar", "glob": "scalar"},
    "git_read": {"repo": "scalar", "path": "scalar", "paths": "items"},
    "git": {"path": "scalar", "paths": "items", "files": "items"},
    "exec": {"cwd": "scalar"},
    "tool_pipeline": {"cwd": "scalar"},
    "lint_repair": {"cwd": "scalar"},
    "docker": {"file": "scalar"},
    "shellcheck": {"file": "scalar", "files": "items"},
    "diff_files": {"path_a": "scalar", "path_b": "scalar"},
}

PATH_KEY = "x-daimonos-path"


def _marked_arguments(input_schema) -> dict:
    """Return {property: kind} for every argument of ``input_schema`` that
    carries ``x-daimonos-path: true``. ``kind`` is ``scalar`` when the property
    node is marked, ``items`` when the array's ``items`` node is marked."""
    marked = {}
    for name, node in (input_schema.get("properties") or {}).items():
        if not isinstance(node, dict):
            continue
        if node.get(PATH_KEY) is True:
            marked[name] = "scalar"
        items = node.get("items")
        if isinstance(items, dict) and items.get(PATH_KEY) is True:
            marked[name] = "items"
    return marked


def test_ac_15_1_every_full_schema_is_self_contained_2020_12(daimonos_binary, tmp_path):
    """Slice 04 / AC-15.1: in full-schema mode every schema in the dump
    validates against the 2020-12 metaschema and contains no ``$ref`` to
    anything outside itself.

    The metaschema check is authoritative: the dump runs the ADR-024
    Draft-2020-12 validator (``schema_contract``) over every served schema and
    refuses the run with ``schema_unusable`` if any is not valid self-contained
    2020-12, so a zero exit *is* the metaschema pass (ADR-030). The
    no-external-``$ref`` half is checked here independently by walking the JSON,
    and every tool must carry a real (non-stripped) schema."""
    (tmp_path / ".git").mkdir()  # also expose the context-gated `git` tool
    proc, doc = _dump(daimonos_binary, tmp_path, "--full-tool-schemas")
    assert proc.returncode == 0, (
        "full-schema dump was refused (schema_unusable or other):\n"
        f"stderr={proc.stderr}"
    )
    assert doc["tools"], "empty tools array"
    for tool in doc["tools"]:
        schema = tool["input_schema"]
        assert schema != {"type": "object"}, f"{tool['name']} still stripped in full mode"
        assert not _external_refs(schema), (
            f"{tool['name']} references outside itself: {_external_refs(schema)}"
        )


def test_full_schema_dump_marks_every_owned_tool_path(daimonos_binary, tmp_path):
    """Slice 04 (integrator-required): with the mode on, the ``x-daimonos-path``
    marks in the dump equal the independently-stated owned-path inventory, for
    every tool present — not only the handful whose schema the default dump
    already serves. Before WP17 the 14 stripped tools hid their marks behind
    ``get_tool_schema``; with full schemas the dump itself carries them."""
    (tmp_path / ".git").mkdir()  # expose `git`
    proc, doc = _dump(daimonos_binary, tmp_path, "--full-tool-schemas")
    assert proc.returncode == 0, proc.stderr
    tools = {t["name"]: t for t in doc["tools"]}

    for name, tool in tools.items():
        expected = _PATH_INVENTORY.get(name, {})
        actual = _marked_arguments(tool["input_schema"])
        assert actual == expected, (
            f"{name}: marks in the full dump {actual} != inventory {expected}"
        )

    # Guard against a vacuous pass: the core owned-path tools must be present and
    # marked in the full dump (including ones the default dump used to strip).
    for required in ("read_file", "search", "ls", "set_cwd", "exec", "git_read", "shellcheck"):
        assert required in tools, f"{required} missing from the full dump"
        assert _marked_arguments(tools[required]["input_schema"]), (
            f"{required} carries no path mark in the full dump"
        )


def test_opt_out_default_dump_keeps_stripped_placeholders(daimonos_binary, tmp_path):
    """Slice 04 (opt-out regression): with neither the flag nor the env set, the
    default dump still serves the stripped ``{"type": "object"}`` placeholder for
    Terse-tier tools and the full schema for Full-tier tools — WP17 changes
    nothing outside its opt-in mode (checkpoint: default stripped/full behavior
    unchanged)."""
    clean_env = {k: v for k, v in os.environ.items() if k != "DAIMONOS_MCP_FULL_SCHEMAS"}
    proc, doc = _dump(daimonos_binary, tmp_path, env=clean_env)
    assert proc.returncode == 0, proc.stderr
    assert doc["effective_options"]["full_tool_schemas"] is False
    tools = {t["name"]: t for t in doc["tools"]}

    present_terse = [n for n in _ALWAYS_EXPOSED_TERSE_TOOLS if n in tools]
    assert present_terse, "no always-exposed Terse tool present; test would be vacuous"
    for name in present_terse:
        assert tools[name]["input_schema"] == {"type": "object"}, (
            f"{name} should be stripped by default but is {tools[name]['input_schema']}"
        )

    # A Full-tier tool keeps its full schema by default (unchanged behavior).
    assert "read_file" in tools
    assert tools["read_file"]["input_schema"].get("properties"), (
        "read_file (Full tier) lost its default full schema"
    )


def test_opt_in_populates_the_same_tools_the_default_strips(daimonos_binary, tmp_path):
    """Slice 04: the exact tools stripped by default (previous test) carry a
    populated schema once the mode is on — the observable opt-in effect."""
    proc, doc = _dump(daimonos_binary, tmp_path, "--full-tool-schemas")
    assert proc.returncode == 0, proc.stderr
    tools = {t["name"]: t for t in doc["tools"]}
    present_terse = [n for n in _ALWAYS_EXPOSED_TERSE_TOOLS if n in tools]
    assert present_terse
    for name in present_terse:
        assert tools[name]["input_schema"] != {"type": "object"}, (
            f"{name} should carry its full schema in full mode"
        )


# ---------------------------------------------------------------------------
# Frontend coverage (WP17): the option must be honored on every frontend that
# accepts it, never silently ignored. `--full-tool-schemas` is a serving/export
# control: the MCP server serves full schemas; the `tool-contract` dump exports
# them (above); the model-running frontends honor it by enforcing the ADR-030
# self-contained gate before the first model request, and the session daemon
# carries it in its launch identity (src/session_bootstrap.rs unit tests).
# ---------------------------------------------------------------------------


def _mcp_list_tools(daimonos_binary, workspace, *extra_args):
    """Spawn an MCP stdio server, handshake, and return {name: tool}."""
    from conftest import DaimonosClient

    proc = subprocess.Popen(
        [daimonos_binary, *extra_args, "--mcp", "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        client = DaimonosClient(proc, str(workspace))
        init = client.send_raw({
            "jsonrpc": "2.0",
            "id": client._next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1.0.0"},
            },
        })
        assert "result" in init, f"initialize failed: {init}"
        client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return {t["name"]: t for t in client.list_tools()}
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def test_mcp_server_honors_full_tool_schemas_flag(daimonos_binary, tmp_path):
    """Frontend — MCP server: with ``--full-tool-schemas`` the live ``tools/list``
    serves a populated schema for a Terse-tier tool that the default server
    strips. Proves the flag reaches the serving path, not only the dump."""
    default = _mcp_list_tools(daimonos_binary, tmp_path)
    assert default.get("workspace_info", {}).get("inputSchema") == {"type": "object"}, (
        "workspace_info should be stripped by the default MCP server"
    )

    full = _mcp_list_tools(daimonos_binary, tmp_path, "--full-tool-schemas")
    ws = full.get("workspace_info")
    assert ws is not None, "workspace_info missing from full-schema tools/list"
    assert ws["inputSchema"] != {"type": "object"}, (
        "workspace_info should carry its full schema with --full-tool-schemas"
    )


def test_agent_frontend_honors_full_tool_schemas_flag(daimonos_binary, tmp_path):
    """Frontend — agent: ``agent --dry-run --full-tool-schemas`` is accepted and
    runs the ADR-030 self-contained gate over the model-facing catalog before
    any request, rather than rejecting the flag or ignoring it. The native
    catalog is self-contained, so the run proceeds (exit 0); an unusable schema
    would exit non-zero with ``schema_unusable`` (proven at the Rust seam)."""
    isolated = {
        k: v for k, v in os.environ.items()
        if not k.startswith(("DAIMONOS_", "XDG_"))
    }
    isolated.update(HOME=str(tmp_path / "home"))
    (tmp_path / "home").mkdir()

    proc = subprocess.run(
        [daimonos_binary, "-w", str(tmp_path), "agent", "--dry-run",
         "--full-tool-schemas", "a task"],
        capture_output=True,
        text=True,
        timeout=60,
        env=isolated,
    )
    assert proc.returncode == 0, (
        "agent --dry-run --full-tool-schemas must be honored (gate passes for the "
        f"native catalog), got exit {proc.returncode}\nstderr={proc.stderr}"
    )
    assert "schema_unusable" not in proc.stderr


def test_agent_help_lists_full_tool_schemas_flag(daimonos_binary):
    """A missing flag on a frontend is a hard failure, not a skip: the global
    ``--full-tool-schemas`` option is advertised in help."""
    completed = subprocess.run(
        [daimonos_binary, "agent", "--help"],
        capture_output=True, text=True, timeout=30,
    )
    text = (completed.stdout or "") + (completed.stderr or "")
    assert "--full-tool-schemas" in text
