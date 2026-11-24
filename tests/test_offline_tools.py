"""FR-18 / ADR-032: analytics control is independent of network policy."""
import json
import os
import pty
import sqlite3
import subprocess
import time

from test_agent_env_precedence import stop_session_daemons
from test_agent_oneshot_sessions import Harness


def test_analytics_off_skips_collection_with_an_enabled_config(daimonos_factory, tmp_path):
    workspace = tmp_path / "repo"
    workspace.mkdir()
    (workspace / "sample").write_text("hello from the workspace")
    for off in (True, False):
        root = tmp_path / ("off" if off else "on")
        root.mkdir()
        database = root / "analytics.db"
        config = root / "config.toml"
        config.write_text(f"[analytics]\nenabled = true\ndb_path = {json.dumps(str(database))}\n")
        args = ["--config", str(config), "--ignore-config-files"]
        if off:
            args.append("--analytics-off")
        client = daimonos_factory(workspace, args=args, env={"HOME": str(root)})
        read = client.call_tool("read_file", {"path": "sample"})
        assert "hello from the workspace" in read["content"][0]["text"]
        info = json.loads(client.call_tool("workspace_info")["content"][0]["text"])
        assert (info.get("analytics") is not None) == (not off)
        assert database.exists() == (not off)


def test_analytics_off_leaves_an_existing_database_untouched(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "sample").write_text("hello")
    database = tmp_path / "existing.db"
    with sqlite3.connect(database) as connection:
        connection.execute("CREATE TABLE sentinel(value TEXT)")
        connection.execute("INSERT INTO sentinel VALUES ('preserve')")
    connection.close()
    before = database.read_bytes()
    config = tmp_path / "config.toml"
    config.write_text(f"[analytics]\nenabled = true\ndb_path = {json.dumps(str(database))}\n")
    client = daimonos_factory(workspace, args=["--ignore-config-files", "--config", str(config), "--analytics-off"],
                              env={"HOME": str(tmp_path)})
    assert "hello" in client.call_tool("read_file", {"path": "sample"})["content"][0]["text"]
    assert client.call_tool("session_stats")["isError"]
    client.process.stdin.close()
    assert client.process.wait(timeout=10) == 0
    assert database.read_bytes() == before
    assert not list(tmp_path.glob("existing.db-*"))


def test_analytics_off_survives_interactive_bootstrap(daimonos_binary, tmp_path):
    # Wait until the *child* starts serving. The frontend and child each load config.
    harness = Harness(daimonos_binary, tmp_path, 1)
    socket = tmp_path / "session.sock"
    db = tmp_path / "analytics.db"
    harness.config.write_text(f'[session]\nsocket_path = "{socket}"\n'
                              f'[analytics]\nenabled = true\ndb_path = "{db}"\n')
    master, slave = pty.openpty()
    try:
        with subprocess.Popen(harness.command("--interactive", global_args=["--analytics-off"]),
                              cwd=harness.workspace, env={**harness.environment(), "TERM": "xterm"},
                              stdin=slave, stdout=slave, stderr=subprocess.PIPE, text=True) as frontend:
            try:
                deadline = time.monotonic() + 15
                while not (socket.is_socket() and socket.with_suffix(".sock.pid").exists()):
                    assert frontend.poll() is None, frontend.stderr.read()
                    assert time.monotonic() < deadline, "daemon did not start"
                    time.sleep(0.02)
                assert not db.exists()
            finally:
                frontend.kill()
                frontend.wait(timeout=5)
    finally:
        os.close(slave)
        os.close(master)
        stop_session_daemons(socket)
    assert not db.exists()
