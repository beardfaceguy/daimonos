"""Tests for token analytics and session_stats tool (CLA-297)."""

import json
import time


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
