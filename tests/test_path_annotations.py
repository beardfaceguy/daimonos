"""WP16 (FR-14): owned path annotations in the effective tool contract.

Context: docs/adr/029-effective-tool-contract.md and the reviewer-runtime
INTERFACES.md WP16 rows. These are process-level acceptance tests driving a real
``daimonos`` binary through the ``tool-contract`` dump (FR-13, WP15), the
acceptance surface named by AC-14.1.

AC-14.1 (all marked): for every tool in the dump, every argument that accepts a
path is marked with the standard annotation ``"x-daimonos-path": true`` —
including path elements inside arrays — and no non-path argument is marked.

A path annotation describes *semantics*, not an argument merely named ``path``:
opaque command/script strings, agent-mail reservation glob patterns (never
resolved against the filesystem), URLs, revisions, search patterns, and
identifiers are deliberately NOT marked.
"""

import json
import os
import subprocess

PATH_KEY = "x-daimonos-path"


def _dump(daimonos_binary, workspace, env=None) -> dict:
    """Run ``daimonos tool-contract`` for ``workspace`` and return parsed JSON.

    ``--workspace`` is a root option, so it precedes the subcommand.
    """
    proc = subprocess.run(
        [daimonos_binary, "-w", str(workspace), "tool-contract"],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    assert proc.returncode == 0, (
        f"tool-contract exited {proc.returncode}\n"
        f"stdout={proc.stdout}\nstderr={proc.stderr}"
    )
    return json.loads(proc.stdout)


def _dump_full(daimonos_binary, workspace):
    """Dump with full tool schemas forced, so Terse-tier schemas are populated
    (dbunk always runs full-schema mode, FR-15). Uses the existing
    ``DAIMONOS_MCP_FULL_SCHEMAS`` env seam."""
    env = {**os.environ, "DAIMONOS_MCP_FULL_SCHEMAS": "1"}
    return _dump(daimonos_binary, workspace, env=env)


def _tools_by_name(doc) -> dict:
    return {t["name"]: t for t in doc["tools"]}


def test_scalar_path_arguments_are_marked(daimonos_binary, tmp_path):
    """Slice 01: scalar filesystem-path properties of the core file tools carry
    ``x-daimonos-path: true``; their non-path scalar properties do not.

    These are Full-tier tools, so the default dump already carries their full
    schemas (no full-schema mode needed)."""
    tools = _tools_by_name(_dump(daimonos_binary, tmp_path))

    # (tool, path properties, non-path properties)
    cases = [
        ("read_file", ["path"], ["offset", "limit"]),
        ("write_file", ["path"], ["content"]),
        ("append_file", ["path"], ["content"]),
        ("edit_file", ["path"], ["edits"]),
        ("search", ["path", "glob"], ["pattern", "mode", "max_results"]),
    ]
    for name, path_args, nonpath_args in cases:
        assert name in tools, f"{name} missing from the default dump"
        props = tools[name]["input_schema"]["properties"]
        for arg in path_args:
            assert props[arg].get(PATH_KEY) is True, (
                f"{name}.{arg} is a filesystem path but is not marked "
                f"{PATH_KEY}: {props[arg]}"
            )
        for arg in nonpath_args:
            assert PATH_KEY not in props[arg], (
                f"{name}.{arg} is not a path but is marked {PATH_KEY}: "
                f"{props[arg]}"
            )


def test_array_path_elements_marked_and_reservation_globs_not(daimonos_binary, tmp_path):
    """Slice 02: array-of-path arguments mark their *elements* (the ``items``
    node), while opaque agent-mail reservation glob arrays are left unmarked.

    This exercises the "semantics, not spelling" rule: ``git_read.paths`` and
    ``git.paths``/``git.files`` are filesystem paths, but
    ``reserve_paths.paths`` / ``release_reservations.paths`` /
    ``check_conflicts.paths`` are opaque reservation keys never resolved against
    the filesystem, so they must not be marked even though they are spelled
    ``paths``."""
    (tmp_path / ".git").mkdir()  # expose the context-gated `git` tool
    tools = _tools_by_name(_dump_full(daimonos_binary, tmp_path))

    # git_read: scalar repo/path marked; array `paths` marks its elements; the
    # array node itself is not marked; revisions/patterns are not paths.
    gr = tools["git_read"]["input_schema"]["properties"]
    assert gr["repo"].get(PATH_KEY) is True
    assert gr["path"].get(PATH_KEY) is True
    assert gr["paths"]["items"].get(PATH_KEY) is True, gr["paths"]
    assert PATH_KEY not in gr["paths"], "array node marked; expected only its items"
    for non_path in ("rev", "pattern", "a", "b"):
        assert PATH_KEY not in gr[non_path], f"git_read.{non_path} wrongly marked"

    # git: structured path args marked (arrays on their items); the raw `args`
    # argv is opaque and must carry no mark anywhere.
    g = tools["git"]["input_schema"]["properties"]
    assert g["path"].get(PATH_KEY) is True
    assert g["paths"]["items"].get(PATH_KEY) is True
    assert g["files"]["items"].get(PATH_KEY) is True
    assert PATH_KEY not in json.dumps(g["args"]), "opaque git argv marked as path"
    for non_path in ("command", "message", "branch", "remote"):
        assert PATH_KEY not in g[non_path], f"git.{non_path} wrongly marked"

    # Agent-mail reservation globs: opaque, never filesystem-resolved -> unmarked
    # on both the array node and its items.
    for name in ("reserve_paths", "release_reservations", "check_conflicts"):
        props = tools[name]["input_schema"]["properties"]
        paths = props["paths"]
        assert PATH_KEY not in paths, f"{name}.paths wrongly marked"
        assert PATH_KEY not in paths.get("items", {}), f"{name}.paths items wrongly marked"


# The reviewed inventory of owned path arguments, keyed by tool, as the set of
# locations (relative to the tool's input schema) that MUST carry
# ``x-daimonos-path: true`` and no others. A scalar path property is
# ``properties/<name>``; an array-of-paths marks its elements at
# ``properties/<name>/items``. Expected values come from FR-14 + the PM answer
# ("annotate every Daimonos-owned argument whose value is a filesystem path, a
# glob, or a directory to walk; that includes elements of arrays"), not from the
# implementation. Tools absent here (or present with an empty set) must carry no
# mark. On-demand tools (diff_files/tool_pipeline/lint_repair) are excluded from
# the dump by tier and so are not listed.
EXPECTED_MARKS = {
    "read_file": {"properties/path"},
    "write_file": {"properties/path"},
    "append_file": {"properties/path"},
    "edit_file": {"properties/path"},
    "search": {"properties/path", "properties/glob"},
    "exec": {"properties/cwd"},
    "git": {"properties/path", "properties/paths/items", "properties/files/items"},
    "git_read": {"properties/repo", "properties/path", "properties/paths/items"},
    "pytest": {"properties/path"},
    "docker": {"properties/file"},
    "shellcheck": {"properties/file", "properties/files/items"},
    "set_cwd": {"properties/path"},
    "ls": {"properties/path", "properties/glob"},
}


def _marked_locations(schema) -> set:
    """Every location (JSON-pointer-ish, relative to the schema root) whose
    object carries ``x-daimonos-path: true``. Walks the whole schema so a stray
    mark anywhere is caught."""
    found = set()

    def walk(node, pointer):
        if isinstance(node, dict):
            if node.get(PATH_KEY) is True:
                found.add(pointer)
            for key, value in node.items():
                if key == PATH_KEY:
                    continue
                walk(value, f"{pointer}/{key}" if pointer else key)
        elif isinstance(node, list):
            for i, value in enumerate(node):
                walk(value, f"{pointer}/{i}")

    walk(schema, "")
    return found


def test_ac_14_1_every_path_marked_and_nothing_else(daimonos_binary, tmp_path):
    """AC-14.1: for every tool in the dump, every argument that accepts a path
    is marked and no other argument is.

    The workspace is populated so every context-gated tool (git, cargo, pytest,
    docker, gh) is exposed, and full tool schemas are forced so Terse-tier
    schemas are populated. The reviewed inventory then pins the exact marked
    locations for each tool."""
    (tmp_path / ".git").mkdir()
    (tmp_path / "tests").mkdir()
    (tmp_path / "Cargo.toml").write_text("[package]\nname='x'\nversion='0.0.0'\n")
    (tmp_path / "Dockerfile").write_text("FROM scratch\n")

    doc = _dump_full(daimonos_binary, tmp_path)
    tools = _tools_by_name(doc)

    # Every tool the inventory says has path args is actually in the dump, so the
    # test is not silently passing on an absent tool.
    for name in EXPECTED_MARKS:
        assert name in tools, f"{name} expected in the full-schema dump but absent"

    for name, entry in tools.items():
        actual = _marked_locations(entry["input_schema"])
        expected = EXPECTED_MARKS.get(name, set())
        assert actual == expected, (
            f"{name}: path marks do not match the reviewed inventory\n"
            f"  expected: {sorted(expected)}\n"
            f"  actual:   {sorted(actual)}"
        )


def _mcp_contract_version(daimonos_binary, workspace) -> str:
    """Spawn the stdio MCP server and return the contract version reported in
    initialization ``_meta``."""
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(workspace)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        from conftest import DaimonosClient

        client = DaimonosClient(proc, str(workspace))
        resp = client.send_raw({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1.0.0"},
            },
        })
        assert "result" in resp, f"initialize failed: {resp}"
        return resp["result"]["_meta"]["io.daimonos/tool-contract-version"]
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_path_annotations_bump_contract_version(daimonos_binary, tmp_path):
    """Slice 04: adding owned path annotations is an additive (minor) tool
    contract change (ADR-029 / tool-contract-changelog versioning rules), so the
    contract version is 1.1.0 in both the dump and MCP initialization, and the
    changelog records the 1.1.0 entry."""
    doc = _dump(daimonos_binary, tmp_path)
    assert doc["contract_version"] == "1.1.0", (
        f"path annotations are a minor contract change; expected 1.1.0, got "
        f"{doc['contract_version']}"
    )
    assert _mcp_contract_version(daimonos_binary, tmp_path) == "1.1.0"

    # Every version bump must have a changelog entry (ADR-029).
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    changelog = os.path.join(repo_root, "docs", "tool-contract-changelog.md")
    with open(changelog, encoding="utf-8") as f:
        text = f.read()
    assert "[1.1.0]" in text, "tool-contract-changelog.md missing the 1.1.0 entry"
    assert PATH_KEY in text, "changelog 1.1.0 entry should document the annotation key"


def test_annotation_is_additive_and_opaque_args_untouched(daimonos_binary, tmp_path):
    """Slice 06 (opt-out / additive regression): the marker is purely additive
    metadata. A marked path node keeps all its pre-existing schema keywords, an
    array path argument keeps its array/items structure (only the element is
    marked), ``required`` is unchanged, and opaque script/command args carry no
    mark. There is no option to disable the annotation (I1); "unchanged
    behavior" means exactly "additive marker, nothing else altered"."""
    (tmp_path / ".git").mkdir()
    tools = _tools_by_name(_dump_full(daimonos_binary, tmp_path))

    # A marked scalar path keeps its type (and declared description); the mark is
    # added alongside, not in place of, the structural keywords.
    read_file = tools["read_file"]["input_schema"]
    assert read_file["properties"]["path"]["type"] == "string"
    assert read_file["properties"]["path"][PATH_KEY] is True
    assert read_file["required"] == ["path"]  # structure preserved

    # A marked array path keeps array/items structure; only the element carries
    # the mark.
    paths = tools["git"]["input_schema"]["properties"]["paths"]
    assert paths["type"] == "array"
    assert paths["items"]["type"] == "string"
    assert paths["items"][PATH_KEY] is True

    # Opaque script/command arguments are never marked, and keep their types.
    script = tools["execute_script"]["input_schema"]["properties"]
    assert script["code"]["type"] == "string"
    assert PATH_KEY not in json.dumps(tools["execute_script"]["input_schema"])
    assert PATH_KEY not in json.dumps(tools["curl"]["input_schema"])
    assert PATH_KEY not in json.dumps(tools["npm"]["input_schema"])
