"""Tests for token analytics and session_stats tool (CLA-297)."""

import json
import os
import subprocess
import time
import uuid

from conftest import DaimonosClient


def test_session_stats_returns_data_after_tool_calls(daimonos):
    """After a few tool calls, session_stats should report non-zero totals."""
    # Make some tool calls first
    daimonos.call_tool("write_file", {"path": "a.txt", "content": "hello"})
    daimonos.call_tool("read_file", {"path": "a.txt"})
    daimonos.call_tool("search", {"pattern": "hello"})

    # Allow async analytics writes to complete
    time.sleep(0.3)

    result = daimonos.call_tool("session_stats", {"scope": "session"})
    text = result["content"][0]["text"]
    stats = json.loads(text)

    assert stats["total_calls"] >= 3, f"expected >=3 calls, got {stats['total_calls']}"
    assert stats["total_request_tokens"] > 0
    assert stats["total_response_tokens"] > 0
    assert "per_tool" in stats
    assert "read_file" in stats["per_tool"]
    assert "write_file" in stats["per_tool"]


def test_session_stats_history_scope(daimonos):
    """History scope queries SQLite for cross-session data."""
    daimonos.call_tool("read_file", {"path": "nonexistent.txt"})
    time.sleep(0.3)

    result = daimonos.call_tool("session_stats", {"scope": "history", "days": 30})
    text = result["content"][0]["text"]
    data = json.loads(text)

    assert "total_calls" in data
    assert "sessions" in data
    assert "top_tools" in data
    assert data["days"] == 30


def test_session_stats_daily_scope(daimonos):
    """Daily scope returns trend data."""
    daimonos.call_tool("write_file", {"path": "b.txt", "content": "test"})
    time.sleep(0.3)

    result = daimonos.call_tool("session_stats", {"scope": "daily", "days": 7})
    text = result["content"][0]["text"]
    data = json.loads(text)

    assert isinstance(data, list)
    if len(data) > 0:
        assert "date" in data[0]
        assert "calls" in data[0]
        assert "response_tokens" in data[0]


def test_session_stats_invalid_scope(daimonos):
    """Invalid scope returns an error."""
    result = daimonos.call_tool("session_stats", {"scope": "invalid"})
    assert result.get("isError") is True


def test_workspace_info_includes_analytics(daimonos):
    """workspace_info should include an analytics summary after tool calls."""
    daimonos.call_tool("write_file", {"path": "c.txt", "content": "data"})
    time.sleep(0.3)

    result = daimonos.call_tool("workspace_info", {})
    text = result["content"][0]["text"]
    info = json.loads(text)

    assert "analytics" in info, "workspace_info should include analytics"
    analytics = info["analytics"]
    assert analytics["calls"] >= 1
    assert "resp_tokens" in analytics
    assert "redirects" in analytics
    assert "db_path" in analytics
    assert isinstance(analytics["db_path"], str)


def test_session_stats_tracks_read_dedup(daimonos):
    """Reading the same file twice should trigger a dedup hit in analytics."""
    daimonos.call_tool("write_file", {"path": "dup.txt", "content": "same content"})
    daimonos.call_tool("read_file", {"path": "dup.txt"})
    daimonos.call_tool("read_file", {"path": "dup.txt"})
    time.sleep(0.3)

    result = daimonos.call_tool("session_stats", {"scope": "session"})
    stats = json.loads(result["content"][0]["text"])

    assert stats["dedup_hits"] >= 1, "second read of same file should be a dedup hit"


def test_session_stats_per_tool_breakdown(daimonos):
    """Per-tool breakdown should show individual tool stats."""
    daimonos.call_tool("write_file", {"path": "x.txt", "content": "x"})
    daimonos.call_tool("write_file", {"path": "y.txt", "content": "y"})
    daimonos.call_tool("read_file", {"path": "x.txt"})
    time.sleep(0.3)

    result = daimonos.call_tool("session_stats", {"scope": "session"})
    stats = json.loads(result["content"][0]["text"])

    per_tool = stats["per_tool"]
    assert "write_file" in per_tool
    assert per_tool["write_file"]["calls"] >= 2
    assert per_tool["write_file"]["response_tokens"] > 0
    assert "read_file" in per_tool
    assert per_tool["read_file"]["calls"] >= 1


def test_session_stats_in_list_tools(daimonos):
    """session_stats should appear in the default tool list (Terse tier)."""
    tools = daimonos.list_tools()
    tool_names = {t["name"] for t in tools}
    assert "session_stats" in tool_names


# --- external_session_id correlation (vikunja #43) ---


def _spawn_with_env(daimonos_binary, tmp_path, extra_env):
    """Spawn a fresh daimonos subprocess with custom env and complete the
    MCP handshake. Mirrors the conftest fixture but lets each test pick
    its own environment so we can exercise the env-var bootstrap path
    in isolation."""
    env = os.environ.copy()
    env.update(extra_env)
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(tmp_path)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    client = DaimonosClient(proc, str(tmp_path))
    init_resp = client.send_raw({
        "jsonrpc": "2.0",
        "id": client._next_id(),
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1.0.0"},
        },
    })
    assert "result" in init_resp, f"initialize failed: {init_resp}"
    client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})
    return proc, client


def test_external_session_id_bootstraps_from_env_var(daimonos_binary, tmp_path):
    """When DAIMONOS_AGENT_SESSION_ID is set in the launch environment,
    session_stats (session scope) must echo it back so the agent can
    confirm the correlation key was attached."""
    sid = f"claude-pytest-bootstrap-{uuid.uuid4()}"
    proc, client = _spawn_with_env(
        daimonos_binary,
        tmp_path,
        {"DAIMONOS_AGENT_SESSION_ID": sid},
    )
    try:
        result = client.call_tool("session_stats", {"scope": "session"})
        stats = json.loads(result["content"][0]["text"])
        assert stats.get("external_session_id") == sid

        info_text = client.call_tool("workspace_info", {})["content"][0]["text"]
        info = json.loads(info_text)
        assert info["analytics"]["external_session_id"] == sid
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def test_external_session_id_unset_when_env_missing(daimonos):
    """Without the env var (default conftest fixture), session_stats
    should report `external_session_id: null` so the bootstrap path is
    visibly opt-in."""
    result = daimonos.call_tool("session_stats", {"scope": "session"})
    stats = json.loads(result["content"][0]["text"])
    assert "external_session_id" in stats
    assert stats["external_session_id"] is None


def test_set_external_session_id_tool_overrides_and_clears(daimonos):
    """The set_external_session_id MCP tool must mutate the live session
    field; subsequent session_stats reflects the new value, and an empty
    string clears it. The previous value is returned for round-trips."""
    result = daimonos.call_tool(
        "set_external_session_id", {"id": "agent-runtime-XYZ"}
    )
    payload = json.loads(result["content"][0]["text"])
    assert payload["external_session_id"] == "agent-runtime-XYZ"
    assert payload["previous"] is None

    stats = json.loads(
        daimonos.call_tool("session_stats", {"scope": "session"})["content"][0]["text"]
    )
    assert stats["external_session_id"] == "agent-runtime-XYZ"

    cleared = daimonos.call_tool("set_external_session_id", {"id": ""})
    cleared_payload = json.loads(cleared["content"][0]["text"])
    assert cleared_payload["external_session_id"] is None
    assert cleared_payload["previous"] == "agent-runtime-XYZ"

    stats = json.loads(
        daimonos.call_tool("session_stats", {"scope": "session"})["content"][0]["text"]
    )
    assert stats["external_session_id"] is None


def test_session_stats_history_filtered_by_external_session_id(daimonos):
    """Recording rows under one external session id and then querying
    history with that filter must isolate just those rows. Switches sid
    mid-session via set_external_session_id, then asserts the filter
    routes through to the SQL `WHERE` clause.

    Uses uuid-suffixed session ids so the test is robust to leftover
    rows in the shared `~/.daimonos/analytics.db`."""
    run = uuid.uuid4()
    sid_a = f"history-filter-A-{run}"
    sid_b = f"history-filter-B-{run}"

    # Tag a batch of operations with sid-A. The
    # `set_external_session_id` call itself runs *after* it mutates the
    # session, so its own analytics row also lands under sid-A.
    daimonos.call_tool("set_external_session_id", {"id": sid_a})
    daimonos.call_tool("write_file", {"path": "a.txt", "content": "hello"})
    daimonos.call_tool("write_file", {"path": "a2.txt", "content": "hi"})

    # Switch and tag the next batch with sid-B.
    daimonos.call_tool("set_external_session_id", {"id": sid_b})
    daimonos.call_tool("write_file", {"path": "b.txt", "content": "hi"})

    # Allow async SQLite writes to land before history reads them.
    time.sleep(0.5)

    only_a = json.loads(
        daimonos.call_tool(
            "session_stats",
            {"scope": "history", "external_session_id": sid_a, "days": 1},
        )["content"][0]["text"]
    )
    only_b = json.loads(
        daimonos.call_tool(
            "session_stats",
            {"scope": "history", "external_session_id": sid_b, "days": 1},
        )["content"][0]["text"]
    )
    nonexistent = json.loads(
        daimonos.call_tool(
            "session_stats",
            {
                "scope": "history",
                "external_session_id": f"does-not-exist-{run}",
                "days": 1,
            },
        )["content"][0]["text"]
    )

    # sid-A: set_external_session_id + 2× write_file = 3 rows
    assert only_a["total_calls"] >= 3, only_a
    # sid-B: set_external_session_id + 1× write_file = 2 rows
    assert only_b["total_calls"] >= 2, only_b
    assert nonexistent["total_calls"] == 0, nonexistent
