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


# ---------------------------------------------------------------------------
# WP17 r2 (Gap 1): supplied (caller-owned final-answer) schema cases under the
# opt-in full-schema mode, driven through a real ``daimonos agent
# --final-output`` process against a local scripted model endpoint on BOTH
# wires (Chat Completions and Messages). ADR-030: "Caller-owned worker and
# final-answer schemas are validated and offered unchanged" and "Detect invalid
# schemas, external references, and recursion locally as ``schema_unusable``".
# INTERFACES.md: under final-output mode ``schema_unusable`` uses exit 20 and
# the final-output error shape.
#
# These tests record every model request body, so S2 can assert the final-tool
# schema offered to the model is deep-equal to the supplied schema (no rewrite).
# ---------------------------------------------------------------------------

import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


BOTH_WIRES = pytest.mark.parametrize("wire", ["chat", "messages"])

# A supplied schema with a RECURSIVE local reference: ``$defs/n`` refers to
# itself. Valid JSON Schema, accepted by a 2020-12 compiler, but unusable in a
# full-schema tool definition (ADR-030; PM A to Q-FR-15-2).
RECURSIVE_SUPPLIED_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {"node": {"$ref": "#/$defs/n"}},
    "$defs": {
        "n": {
            "type": "object",
            "properties": {"child": {"$ref": "#/$defs/n"}},
        }
    },
}

# A supplied schema with a local ``$defs`` and a NON-recursive ``$ref`` (a leaf
# referenced once). Self-contained and acyclic: full-schema mode must accept it
# and offer it to the model unchanged (S2, "offered unchanged").
NONRECURSIVE_DEFS_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "answer": {"type": "string"},
        "id": {"$ref": "#/$defs/id"},
    },
    "required": ["answer", "id"],
    "additionalProperties": False,
    "$defs": {"id": {"type": "string", "minLength": 4}},
}


class _RecordingModelMock:
    """A local scripted model endpoint that records every request body and
    answers one ``final_answer`` tool call with ``answer_args``.

    ``wire`` selects the vendor shape: ``chat`` is OpenAI/OpenRouter Chat
    Completions (SSE; the final tool schema travels in
    ``tools[].function.parameters``), ``messages`` is Anthropic Messages (SSE;
    the schema travels in ``tools[].input_schema``). No real credential or live
    API is used (local fake only).
    """

    def __init__(self, wire, answer_args):
        self.wire = wire
        self.bodies = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                if owner.wire == "chat":
                    payload = json.dumps({"data": [{
                        "id": "test/model", "created": 1, "context_length": 100000,
                        "supported_parameters": ["tools"],
                        "pricing": {"prompt": "0", "completion": "0"},
                    }]}).encode()
                else:
                    payload = json.dumps({"id": "test/model", "type": "model",
                                          "display_name": "Test"}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def _event(self, name, data):
                return ("event: " + name + "\ndata: " + json.dumps(data) + "\n\n").encode()

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                try:
                    owner.bodies.append(json.loads(raw or b"{}"))
                except (ValueError, TypeError):
                    owner.bodies.append({})
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                try:
                    if owner.wire == "chat":
                        delta = {"role": "assistant", "tool_calls": [{
                            "index": 0, "id": "final-1", "type": "function",
                            "function": {"name": "final_answer",
                                         "arguments": json.dumps(answer_args)},
                        }]}
                        chunk = {"choices": [{"index": 0, "delta": delta,
                                              "finish_reason": "tool_calls"}],
                                 "usage": {"prompt_tokens": 10, "completion_tokens": 5}}
                        self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                        self.wfile.write(b"data: [DONE]\n\n")
                    else:
                        blocks = [
                            self._event("content_block_start", {
                                "type": "content_block_start", "index": 0,
                                "content_block": {"type": "tool_use", "id": "final-1",
                                                  "name": "final_answer", "input": {}}}),
                            self._event("content_block_delta", {
                                "type": "content_block_delta", "index": 0,
                                "delta": {"type": "input_json_delta",
                                          "partial_json": json.dumps(answer_args)}}),
                            self._event("message_delta", {
                                "type": "message_delta",
                                "delta": {"stop_reason": "tool_use"},
                                "usage": {"output_tokens": 5}}),
                            self._event("message_stop", {"type": "message_stop"}),
                        ]
                        for b in blocks:
                            self.wfile.write(b)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self):
        return self.server.server_port

    def final_tool_schema(self):
        """The schema offered for the ``final_answer`` tool in the last recorded
        request, read from this wire's tool-list location, or ``None``."""
        for body in reversed(self.bodies):
            for tool in body.get("tools", []) or []:
                if self.wire == "chat":
                    fn = tool.get("function", {})
                    if fn.get("name") == "final_answer":
                        return fn.get("parameters")
                elif tool.get("name") == "final_answer":
                    return tool.get("input_schema")
        return None

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


def _run_final(daimonos_binary, tmp_path, wire, schema, answer_args, *, full_mode,
               extra_env=None):
    """Drive ``daimonos agent --final-output`` once with a supplied ``schema``.

    ``full_mode`` inserts the global ``--full-tool-schemas`` flag before the
    subcommand. ``extra_env`` (WP17 r6) sets additional process environment
    variables after the DAIMONOS_/XDG_ scrub, so a test can pin an explicit
    ``DAIMONOS_MCP_FULL_SCHEMAS`` value against the flag. Returns
    ``(completed, posts, recorded_schema)`` captured before the mock is torn
    down.
    """
    provider = "openrouter" if wire == "chat" else "anthropic"
    mock = _RecordingModelMock(wire, answer_args)
    try:
        home = tmp_path / "home"
        home.mkdir(exist_ok=True)
        config = tmp_path / "config.toml"
        config.write_text("[analytics]\nenabled=false\n[acp.mcp]\nenabled=false\n")
        agent_env = tmp_path / "agent.env"
        agent_env.write_text("\n".join([
            f"DAIMONOS_AGENT_PROVIDER={provider}",
            "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{mock.port}",
            "DAIMONOS_AGENT_API_KEY=synthetic-not-a-real-key",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto",
            "DAIMONOS_AGENT_COMPACTION=off", ""]))
        control = tmp_path / "final_output.json"
        control.write_text(json.dumps({
            "schema": schema, "tool_name": "final_answer",
            "tool_description": "Emit the final structured result.",
            "corrective_retries": 0}))
        argv = [daimonos_binary, "--workspace", str(tmp_path), "--config", str(config)]
        if full_mode:
            argv += ["--full-tool-schemas"]
        argv += ["agent", "--final-output", str(control),
                 "--agent-env", str(agent_env), "--print", "produce the final answer"]
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("DAIMONOS_", "XDG_"))}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"))
        if extra_env:
            env.update(extra_env)
        completed = subprocess.run(argv, capture_output=True, text=True, timeout=120, env=env)
        return completed, len(mock.bodies), mock.final_tool_schema()
    finally:
        mock.close()


def _last_json_line(text):
    lines = [ln for ln in (text or "").strip().splitlines() if ln.strip()]
    return json.loads(lines[-1]) if lines else {}


@BOTH_WIRES
def test_s1_full_mode_refuses_recursive_supplied_schema(daimonos_binary, tmp_path, wire):
    """S1: in full-schema mode a supplied final-answer schema with a recursive
    local reference is refused BEFORE any model request — exit 20, the
    final-output error shape, reason ``schema_unusable`` — and the endpoint
    receives no request (ADR-030 / INTERFACES.md)."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, wire, RECURSIVE_SUPPLIED_SCHEMA,
        {"node": {"child": {}}}, full_mode=True)
    assert completed.returncode == 20, (
        f"recursive supplied schema in full mode must exit 20, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    err = _last_json_line(completed.stderr)
    assert err.get("reason") == "schema_unusable", f"stderr={completed.stderr}"
    assert posts == 0, f"a model request was made before refusal (posts={posts})"


@BOTH_WIRES
def test_s1_default_accepts_recursive_supplied_schema(daimonos_binary, tmp_path, wire):
    """S1 default pin: WITHOUT full-schema mode the same recursive supplied
    schema is accepted exactly as today (exit 0, one model request). This must
    pass before and after the S1 production change — WP17 changes nothing
    outside its opt-in mode."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, wire, RECURSIVE_SUPPLIED_SCHEMA,
        {"node": {"child": {}}}, full_mode=False)
    assert completed.returncode == 0, (
        f"recursive supplied schema must be accepted by default, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    assert posts == 1, f"expected exactly one model request, got {posts}"


@BOTH_WIRES
def test_s2_full_mode_offers_nonrecursive_defs_schema_unchanged(daimonos_binary, tmp_path, wire):
    """S2 ("offered unchanged"): in full-schema mode a supplied schema with a
    local ``$defs`` and a non-recursive ``$ref`` is accepted (exit 0), and the
    final-answer tool schema in the recorded model request is deep-equal to the
    supplied schema — no subset rewriting (ADR-030, ADR-024)."""
    completed, posts, recorded = _run_final(
        daimonos_binary, tmp_path, wire, NONRECURSIVE_DEFS_SCHEMA,
        {"answer": "42", "id": "abcd"}, full_mode=True)
    assert completed.returncode == 0, (
        f"non-recursive $defs schema must be accepted in full mode, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    assert posts == 1, f"expected exactly one model request, got {posts}"
    assert recorded == NONRECURSIVE_DEFS_SCHEMA, (
        f"final-answer schema in the request was rewritten:\n{recorded}")


# A supplied schema using a plain-name ``$anchor`` and a ``#anchor`` reference
# (instead of a JSON-pointer ``$ref``). The S2 direction asks for the same
# "offered unchanged" assertion on an ``$anchor`` schema *if the WP05 validator
# accepts it*.
ANCHOR_SUPPLIED_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "answer": {"type": "string"},
        "id": {"$ref": "#id"},
    },
    "required": ["answer", "id"],
    "additionalProperties": False,
    "$defs": {"id": {"$anchor": "id", "type": "string", "minLength": 4}},
}


@BOTH_WIRES
def test_s2_full_mode_offers_anchor_schema_unchanged(daimonos_binary, tmp_path, wire):
    """S2 ($anchor): the WP05 Draft-2020-12 validator resolves a plain-name
    ``$anchor`` reference, so full-schema mode accepts an ``$anchor`` supplied
    schema (exit 0) and offers it to the model deep-equal/unchanged. Recorded
    here because the direction requested the ``$anchor`` assertion when the
    validator accepts it; it does."""
    completed, posts, recorded = _run_final(
        daimonos_binary, tmp_path, wire, ANCHOR_SUPPLIED_SCHEMA,
        {"answer": "42", "id": "abcd"}, full_mode=True)
    assert completed.returncode == 0, (
        f"$anchor schema must be accepted in full mode, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    assert posts == 1, f"expected exactly one model request, got {posts}"
    assert recorded == ANCHOR_SUPPLIED_SCHEMA, (
        f"$anchor final-answer schema in the request was rewritten:\n{recorded}")


# ---------------------------------------------------------------------------
# WP17 r3 (T3): a supplied final-answer schema that is recursive through a
# reference form the r2 detector did not follow -- a plain-name ``$anchor`` and
# the schema's own ``$id`` -- must be refused in full-schema mode exactly like
# the JSON-pointer recursion of S1: exit 20, ``schema_unusable``, the
# final-output error shape, and no model request. The WP05 Draft-2020-12
# validator accepts each (it resolves the reference), so the refusal is the
# detector's, before any request (ADR-030; integrator r3 direction).
# ---------------------------------------------------------------------------

# Recursive through a plain-name ``$anchor``: ``#node -> #node``.
ANCHOR_RECURSIVE_SUPPLIED_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {"node": {"$ref": "#node"}},
    "$defs": {
        "node": {
            "$anchor": "node",
            "type": "object",
            "properties": {"child": {"$ref": "#node"}},
        }
    },
}

# Recursive through the document's own ``$id``: a property points a ``$ref`` at
# the absolute ``$id`` of the root resource, i.e. back at the whole document.
ID_RECURSIVE_SUPPLIED_SCHEMA = {
    "$id": "https://daimonos.test/final-answer",
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {"loop": {"$ref": "https://daimonos.test/final-answer"}},
}

_T3_RECURSIVE = {
    "anchor": ANCHOR_RECURSIVE_SUPPLIED_SCHEMA,
    "id": ID_RECURSIVE_SUPPLIED_SCHEMA,
}


@BOTH_WIRES
@pytest.mark.parametrize("form", list(_T3_RECURSIVE))
def test_t3_full_mode_refuses_recursive_supplied_schema_by_form(
    daimonos_binary, tmp_path, wire, form
):
    """T3: a supplied schema recursive through ``$anchor`` or through its own
    ``$id`` is refused before any model request in full-schema mode -- exit 20,
    reason ``schema_unusable``, the final-output error shape -- and the endpoint
    receives no request (parity with S1's JSON-pointer recursion)."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, wire, _T3_RECURSIVE[form],
        {"loop": {}, "node": {}}, full_mode=True)
    assert completed.returncode == 20, (
        f"recursive supplied schema (via {form}) in full mode must exit 20, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    err = _last_json_line(completed.stderr)
    assert err.get("reason") == "schema_unusable", f"stderr={completed.stderr}"
    assert posts == 0, f"a model request was made before refusal (posts={posts})"


@BOTH_WIRES
@pytest.mark.parametrize("form", list(_T3_RECURSIVE))
def test_t3_default_accepts_recursive_supplied_schema_by_form(
    daimonos_binary, tmp_path, wire, form
):
    """T3 default pin: WITHOUT full-schema mode the same ``$anchor``/``$id``
    recursive supplied schema is accepted exactly as today (exit 0, one model
    request). WP17 changes nothing outside its opt-in mode."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, wire, _T3_RECURSIVE[form],
        {"loop": {}, "node": {}}, full_mode=False)
    assert completed.returncode == 0, (
        f"recursive supplied schema (via {form}) must be accepted by default, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    assert posts == 1, f"expected exactly one model request, got {posts}"


# ---------------------------------------------------------------------------
# WP17 r4 (integrator decision D46): typed traversal. A reference keyword has
# meaning only in a SCHEMA position. A supplied final-answer schema whose data
# (``const``) merely looks like a ``$ref`` is NOT recursive and is accepted and
# offered unchanged (defect A). A schema that IS recursive through an embedded
# ``$id`` is refused even when a decoy ``$id`` sits in data (defect B). Both are
# driven through a real ``daimonos agent --final-output`` process on both wires.
# ---------------------------------------------------------------------------

# Defect A at the process level: a property's ``const`` holds a ``$ref``-shaped
# object. It is DATA, not a reference, so the schema is self-contained and must
# be accepted in full-schema mode and offered to the model unchanged.
CONST_DATA_SUPPLIED_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
        "answer": {"type": "string"},
        "marker": {"const": {"$ref": "#"}},
    },
    "required": ["answer"],
    "additionalProperties": False,
}

# Defect B at the process level: ``$defs/n`` declares an embedded ``$id`` and
# refers to it (self-recursive), while a sibling property carries
# ``default: {"$id": X}`` as DATA. The data ``$id`` must not mask the real
# resource, so the schema is refused before any model request.
ID_DECOY_RECURSIVE_SUPPLIED_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {"node": {"$ref": "#/$defs/n"}},
    "$defs": {
        "n": {
            "$id": "https://daimonos.test/n",
            "type": "object",
            "properties": {
                "loop": {"$ref": "https://daimonos.test/n"},
                "decoy": {"type": "object", "default": {"$id": "https://daimonos.test/n"}},
            },
        }
    },
}


@BOTH_WIRES
def test_v5_full_mode_accepts_const_data_ref_and_offers_it_unchanged(
    daimonos_binary, tmp_path, wire
):
    """V5 (defect A): a supplied final-answer schema whose ``const`` DATA looks
    like a ``$ref`` is NOT recursive. In full-schema mode it is accepted (exit
    0, one model request) and the final-answer tool schema in the recorded
    request is deep-equal to the supplied schema — the data is not followed and
    the schema is offered unchanged (ADR-030; integrator D46)."""
    completed, posts, recorded = _run_final(
        daimonos_binary, tmp_path, wire, CONST_DATA_SUPPLIED_SCHEMA,
        {"answer": "42"}, full_mode=True)
    assert completed.returncode == 0, (
        f"const-data supplied schema must be accepted in full mode, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    assert posts == 1, f"expected exactly one model request, got {posts}"
    assert recorded == CONST_DATA_SUPPLIED_SCHEMA, (
        f"const-data final-answer schema in the request was rewritten:\n{recorded}")


@BOTH_WIRES
def test_v5_full_mode_refuses_recursive_id_despite_decoy_default_data(
    daimonos_binary, tmp_path, wire
):
    """V5 (defect B): a supplied final-answer schema that IS recursive through an
    embedded ``$id`` is refused before any model request in full-schema mode —
    exit 20, reason ``schema_unusable`` — even though a decoy ``$id`` sits in a
    ``default`` DATA value. The endpoint receives no request (ADR-030; D46)."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, wire, ID_DECOY_RECURSIVE_SUPPLIED_SCHEMA,
        {"node": {}}, full_mode=True)
    assert completed.returncode == 20, (
        f"recursive $id (with decoy data) in full mode must exit 20, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    err = _last_json_line(completed.stderr)
    assert err.get("reason") == "schema_unusable", f"stderr={completed.stderr}"
    assert posts == 0, f"a model request was made before refusal (posts={posts})"


@BOTH_WIRES
def test_v5_default_mode_accepts_recursive_id_despite_decoy(daimonos_binary, tmp_path, wire):
    """V5 default pin: WITHOUT full-schema mode the same recursive-``$id`` schema
    is accepted exactly as today (exit 0, one model request). WP17 changes
    nothing outside its opt-in mode."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, wire, ID_DECOY_RECURSIVE_SUPPLIED_SCHEMA,
        {"node": {}}, full_mode=False)
    assert completed.returncode == 0, (
        f"recursive $id schema must be accepted by default, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    assert posts == 1, f"expected exactly one model request, got {posts}"


# ---------------------------------------------------------------------------
# WP17 r5 (W1): a very deep but FINITE, non-recursive supplied ``$ref`` chain
# must be analyzed without ending the process by a signal. The recursion check
# runs before any model request, so one wire exercises it. Option (a) keeps the
# cycle search iterative (an explicit work list), so a finite acyclic chain is
# accepted (exit 0) in full-schema mode, matching the default placeholder path.
# Context: docs/adr/030-self-contained-full-schemas.md, issues D-WP17-13.
# ---------------------------------------------------------------------------


def _deep_chain_schema(n):
    """A finite, NON-recursive local ``$ref`` chain of ``n`` definitions:
    ``#/$defs/d0 -> properties.n -> $ref d1 -> ... -> d(n-1)`` whose last
    definition is a plain string leaf; the document root references ``d0``.
    Deep in reference hops, shallow in JSON nesting."""
    defs = {}
    for i in range(n):
        if i + 1 == n:
            defs[f"d{i}"] = {"type": "string"}
        else:
            defs[f"d{i}"] = {
                "type": "object",
                "properties": {"n": {"$ref": f"#/$defs/d{i + 1}"}},
            }
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"root": {"$ref": "#/$defs/d0"}},
        "$defs": defs,
    }


@pytest.mark.parametrize("length", [2000, 20000])
def test_w1_full_mode_analyzes_deep_finite_chain_without_a_signal(
        daimonos_binary, tmp_path, length):
    """W1: in full-schema mode a finite, non-recursive supplied final-answer
    chain of 2000 and of 100000 definitions is analyzed WITHOUT the process
    being killed by a signal. The iterative cycle search (option a) accepts the
    acyclic chain, so the return code is non-negative AND zero, and exactly one
    model request is made once the check passes (ADR-030, issues D-WP17-13)."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, "chat", _deep_chain_schema(length),
        {}, full_mode=True)
    assert completed.returncode >= 0, (
        f"deep finite chain of {length} was killed by signal "
        f"{-completed.returncode} in full mode\nstderr={completed.stderr[-2000:]}")
    assert completed.returncode == 0, (
        f"finite non-recursive chain of {length} must be accepted in full "
        f"mode, got {completed.returncode}\nstderr={completed.stderr[-2000:]}")
    assert posts == 1, f"expected exactly one model request, got {posts}"


def test_w1_default_mode_accepts_deep_finite_chain(daimonos_binary, tmp_path):
    """W1 default pin: WITHOUT full-schema mode the same deep finite chain of
    2000 definitions is accepted exactly as today (exit 0, one model request).
    The recursion check does not run outside the opt-in mode; this must hold
    before and after the r5 change."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, "chat", _deep_chain_schema(2000),
        {}, full_mode=False)
    assert completed.returncode == 0, (
        f"finite chain of 2000 must be accepted by default, got "
        f"{completed.returncode}\nstderr={completed.stderr[-2000:]}")
    assert posts == 1, f"expected exactly one model request, got {posts}"


# ---------------------------------------------------------------------------
# WP17 r6 (review R2, integrator decision D53): an explicit ``--full-tool-schemas``
# flag wins over the ``DAIMONOS_MCP_FULL_SCHEMAS`` variable for EVERY value of the
# variable. Before r6 the flag set only the config key, which the env variable
# then silently masked (``--full-tool-schemas`` + ``DAIMONOS_MCP_FULL_SCHEMAS=0``
# gave a stripped dump and ``full_tool_schemas: false``). Without the flag the
# existing precedence (the variable wins over the config key) does not change.
# ---------------------------------------------------------------------------


def test_flag_forces_full_mode_over_env_false_values(daimonos_binary, tmp_path):
    """r6 slice 01: ``--full-tool-schemas`` with ``DAIMONOS_MCP_FULL_SCHEMAS``
    set to a false-y value (``0``/``false``/``off``/empty) still turns the mode
    on — zero stripped placeholders and ``full_tool_schemas: true`` — instead of
    letting the env silently mask the explicit flag (D53)."""
    for value in ("0", "false", "off", ""):
        env = {k: v for k, v in os.environ.items() if k != "DAIMONOS_MCP_FULL_SCHEMAS"}
        env["DAIMONOS_MCP_FULL_SCHEMAS"] = value
        proc, doc = _dump(daimonos_binary, tmp_path, "--full-tool-schemas", env=env)
        assert proc.returncode == 0, (
            f"env {value!r}: tool-contract exited {proc.returncode}\n"
            f"stdout={proc.stdout}\nstderr={proc.stderr}")
        assert doc["effective_options"]["full_tool_schemas"] is True, (
            f"--full-tool-schemas must win over DAIMONOS_MCP_FULL_SCHEMAS={value!r}")
        stripped = [t["name"] for t in doc["tools"]
                    if t["input_schema"] == {"type": "object"}]
        assert not stripped, (
            f"env {value!r}: the explicit flag was ignored; stripped "
            f"placeholders remain: {stripped}")


def test_env_zero_still_wins_over_config_key_without_the_flag(daimonos_binary, tmp_path):
    """r6 precedence pin (passes before and after the r6 change): WITHOUT the
    flag, ``DAIMONOS_MCP_FULL_SCHEMAS=0`` still wins over ``[mcp]
    full_tool_schemas = true`` in the config. The r6 change only adds
    flag-over-env precedence; it must not disturb the existing env-over-config
    precedence (D53)."""
    config = tmp_path / "daimonos.toml"
    config.write_text("[mcp]\nfull_tool_schemas = true\n")
    env = {k: v for k, v in os.environ.items() if k != "DAIMONOS_MCP_FULL_SCHEMAS"}
    env["DAIMONOS_MCP_FULL_SCHEMAS"] = "0"
    proc = subprocess.run(
        [daimonos_binary, "--config", str(config), "-w", str(tmp_path), "tool-contract"],
        capture_output=True, text=True, timeout=60, env=env)
    assert proc.returncode == 0, proc.stderr
    doc = json.loads(proc.stdout)
    assert doc["effective_options"]["full_tool_schemas"] is False, (
        "without the flag, DAIMONOS_MCP_FULL_SCHEMAS=0 must still win over the "
        "[mcp] full_tool_schemas=true config key")


def test_flag_over_env_zero_refuses_recursive_supplied_schema(daimonos_binary, tmp_path):
    """r6 slice 01 (final-output frontend): ``--full-tool-schemas`` with
    ``DAIMONOS_MCP_FULL_SCHEMAS=0`` and a recursive supplied final-answer schema
    still refuses the run before any model request (exit 20, ``schema_unusable``,
    zero posts). The flag forces full mode on despite the false-y env, so the
    ADR-030 gate runs — proving the precedence fix reaches the model-running
    frontend, not only the dump."""
    completed, posts, _ = _run_final(
        daimonos_binary, tmp_path, "chat", RECURSIVE_SUPPLIED_SCHEMA,
        {"node": {"child": {}}}, full_mode=True,
        extra_env={"DAIMONOS_MCP_FULL_SCHEMAS": "0"})
    assert completed.returncode == 20, (
        f"flag + env 0 recursive schema must exit 20, got "
        f"{completed.returncode}\nstderr={completed.stderr}")
    err = _last_json_line(completed.stderr)
    assert err.get("reason") == "schema_unusable", f"stderr={completed.stderr}"
    assert posts == 0, f"a model request was made before refusal (posts={posts})"
