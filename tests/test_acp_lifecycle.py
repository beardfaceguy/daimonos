import signal
import subprocess
import time

import pytest


def _write_agent_env(tmp_path):
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


def _spawn_acp(daimonos_binary, tmp_path):
    return subprocess.Popen(
        [daimonos_binary, "acp", "--agent-env", str(_write_agent_env(tmp_path))],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
    process = _spawn_acp(daimonos_binary, tmp_path)

    process.stdin.close()

    try:
        assert process.wait(timeout=5) == 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()


@pytest.mark.parametrize("sig", [signal.SIGTERM, signal.SIGINT, signal.SIGHUP])
def test_acp_runs_teardown_when_signaled(daimonos_binary, tmp_path, sig):
    """Signals take the EOF teardown path instead of the default handler (#1544).

    The default handler exits with -signal and skips bridge/tool teardown,
    orphaning running tool process groups; a clean 0 proves teardown ran.
    """
    process = _spawn_acp(daimonos_binary, tmp_path)
    try:
        time.sleep(1.0)
        assert process.poll() is None, process.stderr.read().decode()
        process.send_signal(sig)
        assert process.wait(timeout=10) == 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
