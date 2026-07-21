import json
import stat
import subprocess


def test_structured_log_file_contains_startup_event(daimonos_binary, tmp_path):
    log_dir = tmp_path / "logs"
    config = tmp_path / "daimonos.toml"
    config.write_text(
        """
[logging]
enabled = true
level = "info"
stderr_level = "off"
directory = "{}"
file_prefix = "integration"
rotation = "never"
max_files = 2
resource_interval_secs = 0

[analytics]
enabled = false
""".format(log_dir.as_posix())
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
    assert completed.returncode == 0, completed.stderr

    log_files = list(log_dir.iterdir())
    assert len(log_files) == 1
    assert stat.S_IMODE(log_dir.stat().st_mode) == 0o700
    assert stat.S_IMODE(log_files[0].stat().st_mode) == 0o600
    events = [json.loads(line) for line in log_files[0].read_text().splitlines()]
    startup = next(event for event in events if event["fields"].get("event") == "process_start")
    assert startup["fields"]["mode"] == "stats"
    assert startup["fields"]["workspace"] == str(tmp_path)
    assert any(event["fields"].get("event") == "process_stop" for event in events)
