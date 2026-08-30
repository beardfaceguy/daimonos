import json
import os
import subprocess


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
