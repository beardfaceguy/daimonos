"""Tests for notifications/tools/list_changed (vikunja #993).

The server advertises `tools.list_changed: true` and must emit the
notification when lazy exposure grows the visible tool set (e.g. via
`list_all_tools` or first use of an on-demand tool).
"""
from __future__ import annotations

LIST_CHANGED = "notifications/tools/list_changed"


def test_list_all_tools_emits_list_changed(daimonos):
    # Baseline: on-demand tools are hidden until activated.
    before = {t["name"] for t in daimonos.list_tools()}
    assert "diff_files" not in before

    # Activating all tools grows the exposed set.
    daimonos.call_tool("list_all_tools")
    # A follow-up round-trip flushes any notification that trailed the response.
    after = {t["name"] for t in daimonos.list_tools()}
    assert "diff_files" in after

    notes = daimonos.drain_notifications(LIST_CHANGED)
    assert notes, f"expected a {LIST_CHANGED} notification, got {daimonos.notifications}"
    assert notes[-1].get("method") == LIST_CHANGED


def test_first_use_of_on_demand_tool_emits_list_changed(daimonos):
    daimonos.notifications.clear()
    # diff_files is on-demand; calling it activates it even if the args error.
    daimonos.call_tool("diff_files", {"a": "x", "b": "y"})
    daimonos.list_tools()  # flush

    notes = daimonos.drain_notifications(LIST_CHANGED)
    assert notes, f"expected a {LIST_CHANGED} after first on-demand use, got {daimonos.notifications}"


def test_no_notification_for_already_exposed_tool(daimonos):
    # workspace_info is a core tool, exposed from the start.
    daimonos.notifications.clear()
    daimonos.call_tool("workspace_info")
    daimonos.call_tool("workspace_info")
    daimonos.list_tools()  # flush any stragglers

    notes = daimonos.drain_notifications(LIST_CHANGED)
    assert not notes, f"unexpected list_changed for exposed tool: {notes}"
