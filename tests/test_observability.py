import os
import subprocess


def test_missing_otlp_credentials_fail_open(daimonos_binary, tmp_path):
    config = tmp_path / "daimonos.toml"
    config.write_text(
        """
[logging]
enabled = false

[analytics]
enabled = false

[observability]
enabled = true
endpoint = "http://127.0.0.1:9/api/public/otel/v1/traces"
basic_auth_username_env = "DAIMONOS_TEST_MISSING_OTLP_PUBLIC"
basic_auth_password_env = "DAIMONOS_TEST_MISSING_OTLP_SECRET"
""",
        encoding="utf-8",
    )
    env = os.environ.copy()
    env.pop("DAIMONOS_TEST_MISSING_OTLP_PUBLIC", None)
    env.pop("DAIMONOS_TEST_MISSING_OTLP_SECRET", None)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text(
        """
DAIMONOS_AGENT_PROVIDER=openrouter
DAIMONOS_AGENT_MODEL=test-model
DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:9
DAIMONOS_AGENT_API_KEY=test-only-key
DAIMONOS_AGENT_APPROVAL_MODE=auto
DAIMONOS_AGENT_COMPACTION=off
""",
        encoding="utf-8",
    )

    completed = subprocess.run(
        [
            daimonos_binary,
            "--config",
            str(config),
            "--workspace",
            str(tmp_path),
            "acp",
            "--agent-env",
            str(agent_env),
        ],
        input="",
        capture_output=True,
        text=True,
        timeout=15,
        env=env,
    )

    assert completed.returncode == 0
    assert "observability: initialization failed" in completed.stderr
    assert "DAIMONOS_TEST_MISSING_OTLP_PUBLIC" in completed.stderr


def test_non_agent_mode_reports_ignored_observability(daimonos_binary, tmp_path):
    config = tmp_path / "daimonos.toml"
    config.write_text(
        """
[logging]
enabled = false

[analytics]
enabled = false

[observability]
enabled = true
basic_auth = false
""",
        encoding="utf-8",
    )

    completed = subprocess.run(
        [
            daimonos_binary,
            "--config",
            str(config),
            "--workspace",
            str(tmp_path),
            "--stats",
        ],
        capture_output=True,
        text=True,
        timeout=15,
    )

    assert completed.returncode == 0
    assert "observability: ignored for runtime mode 'stats'" in completed.stderr
