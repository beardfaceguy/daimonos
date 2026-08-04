"""Config-gated tool provisioning, exercised through the ``daimonos_factory``
fixture (unified tool provisioning, HANDOFF_unified_tool_provisioning.md).

daimonos reads its config exactly once, at launch, from
``<workspace>/daimonos.toml`` (among other candidates). These tests therefore
need to write that file *before* the process starts and to spawn more than one
process in a single test — precisely what ``daimonos_factory`` provides over the
single-process ``daimonos`` fixture.

Signal under test: ``[mcp] full_tool_schemas``. By default, Terse-tier tools are
advertised with a stripped ``{"type": "object"}`` inputSchema; enabling the flag
makes them advertise their full schema. This is a pure, dependency-free,
launch-time config effect on what ``tools/list`` returns.
"""

from __future__ import annotations

import os


def _has_properties(schema) -> bool:
    """True when a JSON Schema advertises at least one property."""
    return bool((schema or {}).get("properties"))


def test_daimonos_toml_full_tool_schemas_gates_schema_exposure_at_launch(
    daimonos_factory, tmp_path
):
    # `full_tool_schemas` has an env override that would otherwise mask the
    # config we write; clear it so the workspace daimonos.toml is authoritative.
    os.environ.pop("DAIMONOS_MCP_FULL_SCHEMAS", None)

    # Process A: no daimonos.toml -> built-in default (full_tool_schemas = false).
    ws_default = tmp_path / "default"
    ws_default.mkdir()
    default_tools = {t["name"]: t for t in daimonos_factory(ws_default).list_tools()}

    # Process B: same binary, but a daimonos.toml written BEFORE startup opts in.
    ws_full = tmp_path / "full"
    ws_full.mkdir()
    (ws_full / "daimonos.toml").write_text("[mcp]\nfull_tool_schemas = true\n")
    full_tools = {t["name"]: t for t in daimonos_factory(ws_full).list_tools()}

    assert default_tools, "expected a non-empty tool list from the default process"
    # The flag changes *schema exposure*, never which tools are advertised.
    assert set(default_tools) == set(full_tools), (
        "enabling full_tool_schemas must not change the advertised tool set: "
        f"only-default={sorted(set(default_tools) - set(full_tools))} "
        f"only-full={sorted(set(full_tools) - set(default_tools))}"
    )

    # Terse-tier tools are stripped to {"type": "object"} by default.
    stripped_by_default = {
        name
        for name, tool in default_tools.items()
        if not _has_properties(tool.get("inputSchema"))
    }
    assert stripped_by_default, (
        "expected some terse-tier tools to be advertised with a stripped schema "
        "by default"
    )

    # ...and those same tools must gain a full inputSchema once the pre-startup
    # config opts in. This is the launch-time provisioning behaviour under test.
    gained_full_schema = [
        name
        for name in stripped_by_default
        if _has_properties(full_tools[name].get("inputSchema"))
    ]
    assert gained_full_schema, (
        "writing [mcp] full_tool_schemas=true before startup should expose the "
        "full inputSchema for terse-tier tools, but none gained properties "
        f"(default-stripped tools were {sorted(stripped_by_default)})"
    )

    # Spot-check a stable terse-tier tool with no external dependency.
    if "snapshot" in default_tools:
        assert not _has_properties(default_tools["snapshot"].get("inputSchema")), (
            "snapshot should have a stripped schema by default"
        )
        assert _has_properties(full_tools["snapshot"].get("inputSchema")), (
            "snapshot should advertise its full schema when full_tool_schemas=true"
        )
