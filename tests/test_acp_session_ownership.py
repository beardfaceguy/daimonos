"""A live ACP session has one owning process (ADR-033). No real API calls."""
from pathlib import Path

import pytest

from acp_crash_probe import ACP, MockProvider, storage


def _prompt(session_id, text):
    return {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}


def test_second_process_load_is_refused_with_owner_pid(daimonos_binary, tmp_path):
    binary = Path(daimonos_binary)
    provider = MockProvider()
    owner = other = None
    try:
        owner = ACP(binary, tmp_path, provider, "owner")
        workspace = str(tmp_path / "workspace")
        session_id = owner.request("session/new", {"cwd": workspace, "mcpServers": []})["sessionId"]
        owner.request("session/prompt", _prompt(session_id, "first synthetic request"))
        saved = storage(tmp_path, session_id)["message_count"]
        load = {"sessionId": session_id, "cwd": workspace, "mcpServers": []}

        other = ACP(binary, tmp_path, provider, "other")
        with pytest.raises(RuntimeError) as refused:
            other.request("session/load", load)
        assert f"pid {owner.process.pid}" in str(refused.value)

        # The refused load must not take ownership: the owner keeps saving.
        owner.request("session/prompt", _prompt(session_id, "second synthetic request"))
        assert storage(tmp_path, session_id)["message_count"] > saved

        # The OS releases the owner lock when the owning process exits.
        owner.close()
        owner = None
        other.request("session/load", load)
    finally:
        for client in (other, owner):
            if client:
                client.close()
        provider.close()
