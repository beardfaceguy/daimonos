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
        env=env,
    )

    assert completed.returncode == 0
    assert "observability: initialization failed" in completed.stderr
    assert "DAIMONOS_TEST_MISSING_OTLP_PUBLIC" in completed.stderr
