import subprocess


def test_acp_exits_when_stdin_reaches_eof(daimonos_binary, tmp_path):
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
    process = subprocess.Popen(
        [daimonos_binary, "acp", "--agent-env", str(agent_env)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    process.stdin.close()

    try:
        assert process.wait(timeout=5) == 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
