"""Crash recovery through a real ACP process with an isolated synthetic provider."""
import pytest

from acp_crash_probe import experiment


def test_completed_tool_result_survives_forced_termination(daimonos_binary, tmp_path):
    result = experiment(daimonos_binary, tmp_path, "inflight_kill")
    assert "error" not in result, result
    assert result["provider_received_tool_result"], result
    assert result["replayed"]["baseline"], result
    assert result["replayed"]["active_user"], result
    assert result["replayed"]["assistant_step"], result
    assert result["replayed"]["tool_result"], result
    assert not result["replayed"]["partial"], result


@pytest.mark.parametrize("case", ["inflight_term", "inflight_eof", "inflight_cancel", "finished", "completed_kill"])
def test_completed_history_and_termination_controls(daimonos_binary, tmp_path, case):
    result = experiment(daimonos_binary, tmp_path, case)
    assert "error" not in result, result
    assert result["after_shutdown_store"]["integrity"] == "ok", result
    assert result["replayed"]["baseline"], result
    if case != "completed_kill":
        assert result["provider_received_tool_result"], result
        assert result["replayed"]["active_user"], result
        assert result["replayed"]["assistant_step"], result
        assert result["replayed"]["tool_result"], result
    if case.startswith("inflight_"):
        assert not result["replayed"]["partial"], result
    if case == "finished":
        assert result["replayed"]["final"], result
