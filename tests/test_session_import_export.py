import json
import os
import subprocess

import pytest


def run_session(binary, workspace, home, *args):
    env = os.environ.copy()
    env["HOME"] = str(home)
    return subprocess.run(
        [binary, "--workspace", str(workspace), "session", *args],
        capture_output=True,
        text=True,
        timeout=10,
        env=env,
    )


def archive(session_id="imported-session"):
    return {
        "format": "daimonos.session",
        "version": 1,
        "generation": 0,
        "session_id": session_id,
        "model": "test/model",
        "cwd": None,
        "messages": [
            {
                "role": "User",
                "content": [{"Text": "imported question"}],
            }
        ],
    }


def test_session_json_import_export_round_trip(daimonos_binary, tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    source = tmp_path / "session.json"
    source.write_text(json.dumps(archive()))

    imported = run_session(
        daimonos_binary, tmp_path, home, "import", str(source)
    )
    assert imported.returncode == 0, imported.stderr
    assert imported.stdout.strip() == "imported-session"

    exported = run_session(
        daimonos_binary, tmp_path, home, "export", "imported-session"
    )
    assert exported.returncode == 0, exported.stderr
    document = json.loads(exported.stdout)
    assert document["format"] == "daimonos.session"
    assert document["session_id"] == "imported-session"
    assert document["generation"] == 1
    assert document["messages"] == archive()["messages"]


@pytest.mark.parametrize("confined", [False, True])
def test_session_import_export_uses_selected_state_root(daimonos_binary, tmp_path, confined):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    home = tmp_path / "home"
    home.mkdir()
    source = workspace / "session.json"
    source.write_text(json.dumps(archive()))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    prefix = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state)]
    if confined:
        prefix += ["--confine-paths"]
    prefix += ["session"]

    imported = subprocess.run(prefix + ["import", str(source)], capture_output=True, text=True,
                              timeout=10, env=env)
    assert imported.returncode == 0, imported.stderr
    assert imported.stdout.strip() == "imported-session"
    assert (state / "sessions" / "daemon" / "sessions.sqlite3").is_file()
    assert list(home.iterdir()) == []

    exported = subprocess.run(prefix + ["export", "imported-session"], capture_output=True,
                              text=True, timeout=10, env=env)
    assert exported.returncode == 0, exported.stderr
    assert json.loads(exported.stdout)["messages"] == archive()["messages"]
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == [source]


def test_session_state_rejects_database_symlink_escape(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    source = workspace / "session.json"
    source.write_text(json.dumps(archive()))
    outside = workspace / "outside.db"
    outside.write_bytes(b"")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    sessions = state / "sessions" / "daemon"
    sessions.mkdir(parents=True)
    (sessions / "sessions.sqlite3").symlink_to(outside)
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))

    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session", "import", str(source)],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert outside.read_bytes() == b"", "session store wrote outside the state root"
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert not (state / "logs").exists()
    assert list(home.iterdir()) == []


@pytest.mark.parametrize("suffix", ["", "-wal", "-shm", "-journal"])
def test_session_state_rejects_hardlinked_database_files(daimonos_binary, tmp_path, suffix):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    source = workspace / "session.json"
    source.write_text(json.dumps(archive()))
    evidence = workspace / "evidence.db"
    evidence.write_bytes(b"unchanged")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    sessions = state / "sessions" / "daemon"
    sessions.mkdir(parents=True)
    os.link(evidence, sessions / ("sessions.sqlite3" + suffix))
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))

    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "session", "import", str(source)],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert evidence.read_bytes() == b"unchanged"
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "run_configuration_invalid"
    assert error["reason"] == "state_unusable"
    assert not (state / "logs").exists()
    assert list(home.iterdir()) == []


def test_session_import_rejects_duplicate_id_atomically(
    daimonos_binary, tmp_path
):
    home = tmp_path / "home"
    home.mkdir()
    first = tmp_path / "first.json"
    duplicate = tmp_path / "duplicate.json"
    first.write_text(json.dumps(archive()))
    changed = archive()
    changed["model"] = "replacement/model"
    duplicate.write_text(json.dumps(changed))

    assert (
        run_session(daimonos_binary, tmp_path, home, "import", str(first)).returncode
        == 0
    )
    rejected = run_session(
        daimonos_binary, tmp_path, home, "import", str(duplicate)
    )
    assert rejected.returncode != 0
    assert "session id already exists" in rejected.stderr

    exported = run_session(
        daimonos_binary, tmp_path, home, "export", "imported-session"
    )
    assert json.loads(exported.stdout)["model"] == "test/model"


def test_session_export_refuses_to_replace_existing_file(
    daimonos_binary, tmp_path
):
    home = tmp_path / "home"
    home.mkdir()
    source = tmp_path / "session.json"
    output = tmp_path / "export.json"
    source.write_text(json.dumps(archive()))
    output.write_text("keep")
    assert (
        run_session(daimonos_binary, tmp_path, home, "import", str(source)).returncode
        == 0
    )

    rejected = run_session(
        daimonos_binary,
        tmp_path,
        home,
        "export",
        "imported-session",
        "--output",
        str(output),
    )
    assert rejected.returncode != 0
    assert output.read_text() == "keep"


def test_session_import_enforces_configured_archive_limit(
    daimonos_binary, tmp_path
):
    home = tmp_path / "home"
    home.mkdir()
    source = tmp_path / "session.json"
    source.write_text(json.dumps(archive()))
    config = tmp_path / "daimonos.toml"
    config.write_text("[session]\nsession_archive_max_bytes = 8\n")
    env = os.environ.copy()
    env["HOME"] = str(home)

    rejected = subprocess.run(
        [
            daimonos_binary,
            "--workspace",
            str(tmp_path),
            "--config",
            str(config),
            "session",
            "import",
            str(source),
        ],
        capture_output=True,
        text=True,
        timeout=10,
        env=env,
    )

    assert rejected.returncode != 0
    assert "session archive exceeds configured byte limit" in rejected.stderr
