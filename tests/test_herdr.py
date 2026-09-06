"""Herdr supervisor integration: state reporting from the chat frontend.

When daimonos runs inside a herdr pane (HERDR_ENV=1 + HERDR_PANE_ID +
HERDR_BIN_PATH), the chat REPL must report semantic agent state through the
herdr CLI (`pane report-agent` / `pane release-agent`). Outside herdr the
integration must be a complete no-op. These tests substitute a stub script
for the herdr binary and assert the exact calls.
"""

import os
import subprocess


def _agent_env(tmp_path):
    agent_env = tmp_path / "agent.env"
    agent_env.write_text(
        "\n".join(
            [
                "DAIMONOS_AGENT_PROVIDER=openrouter",
                "DAIMONOS_AGENT_MODEL=test/model",
                "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
                "DAIMONOS_AGENT_APPROVAL_MODE=auto",
                "DAIMONOS_AGENT_API_KEY=test",
                "DAIMONOS_AGENT_COMPACTION=off",
                "",
            ]
        )
    )
    return agent_env


def _stub_herdr(tmp_path):
    """Stub herdr binary that appends each invocation's args to a log file."""
    log = tmp_path / "herdr-calls.log"
    stub = tmp_path / "herdr-stub"
    stub.write_text(f'#!/bin/sh\necho "$@" >> {log}\n')
    stub.chmod(0o755)
    return stub, log


def _run_chat(daimonos_binary, tmp_path, extra_env):
    env = dict(os.environ)
    env.update(extra_env)
    # stdin is a pipe, not a TTY: the REPL prints its banner, reports the
    # initial idle state, then fails to read interactive input and exits
    # through the normal shutdown path — which must release herdr authority.
    return subprocess.run(
        [
            daimonos_binary,
            "--workspace",
            str(tmp_path),
            "chat",
            "--agent-env",
            str(_agent_env(tmp_path)),
        ],
        input="exit\n",
        capture_output=True,
        text=True,
        timeout=15,
        env=env,
    )


def test_chat_reports_idle_with_session_id_and_releases_on_exit(
    daimonos_binary, tmp_path
):
    stub, log = _stub_herdr(tmp_path)
    completed = _run_chat(
        daimonos_binary,
        tmp_path,
        {
            "HERDR_ENV": "1",
            "HERDR_PANE_ID": "%42",
            "HERDR_BIN_PATH": str(stub),
        },
    )

    assert "daimonos chat [" in completed.stdout
    calls = log.read_text().splitlines()
    assert len(calls) >= 2, f"expected idle report + release, got: {calls}"

    first, last = calls[0], calls[-1]
    assert "report-agent %42" in first
    assert "--source custom:daimonos" in first
    assert "--state idle" in first
    assert "--agent-session-id" in first, "session id must be published for resume"
    assert "release-agent %42" in last, "exit must release lifecycle authority"
    assert "--source custom:daimonos" in last


def test_chat_outside_herdr_never_invokes_the_binary(daimonos_binary, tmp_path):
    stub, log = _stub_herdr(tmp_path)
    # HERDR_BIN_PATH alone (no HERDR_ENV / pane id) must not activate the
    # integration; also scrub any real herdr env leaking from the test host.
    env = {"HERDR_BIN_PATH": str(stub)}
    host_env = dict(os.environ)
    host_env.pop("HERDR_ENV", None)
    host_env.pop("HERDR_PANE_ID", None)
    host_env.update(env)

    subprocess.run(
        [
            daimonos_binary,
            "--workspace",
            str(tmp_path),
            "chat",
            "--agent-env",
            str(_agent_env(tmp_path)),
        ],
        input="exit\n",
        capture_output=True,
        text=True,
        timeout=15,
        env=host_env,
    )

    assert not log.exists(), f"no herdr calls expected: {log.read_text() if log.exists() else ''}"
