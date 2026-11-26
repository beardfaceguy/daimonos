"""FR-18 / ADR-032: analytics control is independent of network policy."""
import json
import os
import re
import pty
import sqlite3
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

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


@pytest.fixture
def socket_dir():
    # Linux sockaddr_un limits paths to 107 bytes, even when --basetemp is longer.
    with tempfile.TemporaryDirectory(dir="/tmp") as directory:
        yield Path(directory)


def test_analytics_off_survives_interactive_bootstrap(daimonos_binary, tmp_path, socket_dir):
    # Wait until the *child* starts serving. The frontend and child each load config.
    harness = Harness(daimonos_binary, tmp_path, 1)
    socket = socket_dir / "session.sock"
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


def test_offline_refuses_incompatible_modes_before_configuration(daimonos_binary, tmp_path):
    # Deliberately missing config: admission must precede config, discovery, and logging.
    for mode in (["agent", "task"], ["chat"], ["acp"], ["session-daemon"], ["daemon"],
                 ["mcp", "auth", "login", "fixture"]):
        run = subprocess.run([daimonos_binary, "--offline-tools", "--workspace", str(tmp_path),
                              "--config", str(tmp_path / "missing"), *mode],
                             env={"HOME": str(tmp_path), "PATH": os.defpath}, capture_output=True,
                             text=True, stdin=subprocess.DEVNULL, timeout=5)
        assert run.returncode == 2 and run.stdout == "", (mode, run.stderr)
        assert json.loads(run.stderr)["error"]["reason"] == "offline_profile_incompatible"
    assert not list(tmp_path.iterdir())


def test_offline_mcp_serves_only_local_evidence(daimonos_factory, tmp_path):
    (tmp_path / "evidence.txt").write_text("known finding")
    client = daimonos_factory(tmp_path, args=["--offline-tools"], env={"HOME": str(tmp_path)})
    assert {t["name"] for t in client.list_tools()} == {"ls", "read_file", "search", "workspace_info"}
    for name, args, expected in [
        ("ls", {}, "evidence.txt"),
        ("workspace_info", {}, str(tmp_path)),
        ("read_file", {"path": "evidence.txt"}, "known finding"),
        ("search", {"pattern": "known finding"}, "evidence.txt"),
        ("search", {"mode": "files", "pattern": "evidence"}, "evidence.txt"),
    ]:
        result = client.call_tool(name, args)
        assert not result.get("isError"), result
        assert expected in result["content"][0]["text"]


def test_offline_skips_kgl_at_startup_and_during_reads(daimonos_factory, tmp_path):
    graph = tmp_path / "graphify-out" / "graph.json"
    graph.parent.mkdir()
    graph.write_text(json.dumps({"nodes": [{"id": "source", "file_type": "code"}], "links": []}))
    config = tmp_path / "daimonos.toml"
    config.write_text('[analytics]\nenabled = true\n' + f'db_path = "{tmp_path}/analytics.db"\n')
    client = daimonos_factory(tmp_path, args=["--offline-tools"], env={"HOME": str(tmp_path),
                               "DAIMONOS_KGL_AUTOINDEX": "1", "DAIMONOS_KGL_OBSERVE": "1"})
    assert not client.call_tool("read_file", {"path": "graphify-out/graph.json"}).get("isError")
    client.process.stdin.close()
    assert client.process.wait(timeout=10) == 0
    assert not (tmp_path / ".kgl").exists()
    assert not list(tmp_path.glob("analytics.db*"))


def test_offline_refuses_out_of_profile_allowlist_before_serving(daimonos_binary, tmp_path):
    selection = tmp_path / "selection.json"
    selection.write_text('["read_file", "exec"]')
    run = subprocess.run([daimonos_binary, "--offline-tools", "--tool-allowlist", str(selection),
                          "--workspace", str(tmp_path), "mcp"], capture_output=True, text=True,
                         stdin=subprocess.DEVNULL, timeout=5, env={"HOME": str(tmp_path), "PATH": os.defpath})
    assert run.returncode == 2 and run.stdout == "", run.stderr
    assert json.loads(run.stderr.strip().splitlines()[-1])["error"]["reason"] == "offline_profile_incompatible"


def test_offline_contract_matches_the_served_selection(daimonos_binary, daimonos_factory, tmp_path):
    selection = tmp_path / "selected.json"
    selection.write_text('["ls", "read_file"]')
    opts = ["--offline-tools", "--full-tool-schemas", "--tool-allowlist", str(selection)]
    client = daimonos_factory(tmp_path, args=opts, env={"HOME": str(tmp_path)})
    dump = subprocess.run([daimonos_binary, "-w", str(tmp_path), *opts, "tool-contract"],
                          env={"HOME": str(tmp_path), "PATH": os.defpath}, capture_output=True, text=True, timeout=5)
    assert dump.returncode == 0, dump.stderr
    served = {t["name"]: t["inputSchema"] for t in client.list_tools()}
    assert set(served) == {"ls", "read_file"}
    assert {t["name"]: t["input_schema"] for t in json.loads(dump.stdout)["tools"]} == served
    refusal = client.call_tool("search", {"pattern": "absent"})
    assert json.loads(refusal["content"][0]["text"])["code"] == "tool_not_allowed"


# AC-18.1 must count attempts, not just successful connections. Normal CI can
# run the behavioral tests without ptrace. Set DAIMONOS_TEST_STRACE to opt in:
# a missing observer, unsupported namespace, or missing control then fails.
def test_offline_lifecycle_under_network_observer(daimonos_binary, tmp_path):
    import sys

    tracer = os.getenv("DAIMONOS_TEST_STRACE")
    if not tracer:
        pytest.skip("set DAIMONOS_TEST_STRACE to run the syscall acceptance check")
    home = tmp_path / "home"
    home.mkdir()
    ws = tmp_path / "repo"
    ws.mkdir()
    env = {"HOME": str(home), "PATH": os.defpath, "LC_ALL": "C", "DAIMONOS_KGL_AUTOINDEX": "1",
           "DAIMONOS_KGL_OBSERVE": "1", "DAIMONOS_AGENT_BASE_URL": "http://model.invalid",
           "DAIMONOS_AGENT_PROVIDER": "openrouter", "DAIMONOS_AGENT_MODEL": "fixture", "DAIMONOS_AGENT_API_KEY": "fixture"}

    def trace(label, argv, stdin=""):
        log = tmp_path / (label + ".trace")
        completed = subprocess.run(["unshare", "-Urn", tracer, "-f", "-qq", "-s", "192", "-e",
                                    "trace=%network,execve,execveat,io_uring_setup", "-o", str(log), *argv],
                                   input=stdin, capture_output=True, text=True, timeout=45, cwd=ws, env=env)
        return completed, log.read_text()

    # Positive controls: a failed TCP connect and a DNS datagram from a child.
    # Use a literal loopback DNS address, not the host resolver or an external server.
    probe = ("import os,socket; p=os.fork(); "
             "os.waitpid(p,0) if p else (socket.socket().connect_ex(('127.0.0.1',9)), "
             "socket.socket(socket.AF_INET,socket.SOCK_DGRAM).sendto(b'probe',('127.0.0.1',53)))")
    _, positive = trace("positive", [sys.executable, "-c", probe])
    assert re.search(r'connect\(.*AF_INET.*= -1', positive), positive
    assert re.search(r'sendto\(.*htons\(53\).* = -1', positive), positive

    (ws / "evidence.txt").write_text("local evidence")
    # A promisor repo with an absent object: even a plausible git_read request
    # must stop before Git or a helper can try to fill it.
    subprocess.run(["git", "init", "-q", str(ws)], env=env, check=True)
    subprocess.run(["git", "-C", str(ws), "config", "remote.origin.url", "https://missing.invalid/repo"], env=env, check=True)
    subprocess.run(["git", "-C", str(ws), "config", "remote.origin.promisor", "true"], env=env, check=True)
    (ws / "graphify-out").mkdir()
    (ws / "graphify-out" / "graph.json").write_text('{"nodes":[{"id":"f","file_type":"code"}],"links":[]}')
    cfg = ws / "daimonos.toml"
    db = home / "analytics.db"
    cfg.write_text(f'[analytics]\nenabled=true\ndb_path="{db}"\n'
                   '[observability]\nenabled=true\nendpoint="http://telemetry.invalid:4317"\n'
                   '[tool_output.reformat]\nenabled=true\nmin_chars=1\ntools=["ls","read_file","search","workspace_info"]\n'
                   '[tools.custom]\nbin="/bin/false"\nsource_pattern="*.txt"\n')
    args = [daimonos_binary, "-w", str(ws), "--offline-tools", "--config", str(cfg)]
    init = {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "offline", "version": "1"}}
    requests = [("initialize", init), ("tools/list", {})]
    allowed = [("ls", {}), ("read_file", {"path": "evidence.txt"}),
               ("search", {"mode": "files", "pattern": "evidence"}),
               ("search", {"pattern": "local evidence"}), ("workspace_info", {})]
    denied = ["curl", "gh", "git", "git_read", "discord", "exec", "npm", "docker", "cargo", "custom",
              "worker", "kgl_query", "agent_query", "session_stats", "list_all_tools", "get_tool_schema",
              "execute_script", "batch", "tool_pipeline", "unregistered"]
    calls = allowed + [(name, {"command": "show_file", "rev": "a" * 40, "path": "absent"}
                        if name == "git_read" else {}) for name in denied]
    requests.extend(("tools/call", {"name": name, "arguments": params}) for name, params in calls)
    lines = [json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
             for i, (method, params) in enumerate(requests, 1)]
    lines.insert(1, '{"jsonrpc":"2.0","method":"notifications/initialized"}')
    served, activity = trace("session", [*args, "mcp"], "\n".join(lines) + "\n")
    assert served.returncode == 0, served.stderr
    responses = {row["id"]: row["result"] for line in served.stdout.splitlines()
                 if "id" in (row := json.loads(line))}
    assert set(responses) == set(range(1, len(requests) + 1))
    assert {t["name"] for t in responses[2]["tools"]} == {name for name, _ in allowed}
    for i, (name, _) in enumerate(calls, 3):
        result = responses[i]
        if name in denied:
            assert result["isError"]
            assert json.loads(result["content"][0]["text"])["code"] == "offline_tool_forbidden", name
        else:
            assert not result.get("isError"), result
    assert "local evidence" in responses[4]["content"][0]["text"]
    assert not db.exists() and not (ws / ".kgl").exists()

    dump, contract_activity = trace("contract", [*args, "tool-contract"])
    assert dump.returncode == 0 and len(json.loads(dump.stdout)["tools"]) == 4, dump.stderr
    rejected, refusal_activity = trace("model", [*args, "agent", "task"])
    assert rejected.returncode == 2 and json.loads(rejected.stderr)["error"]["reason"] == "offline_profile_incompatible"
    for events in [activity, contract_activity, refusal_activity]:
        # No IP socket, resolver proxy (including Unix-domain connect), asynchronous
        # network submission, or program execution other than the traced binary.
        assert not re.search(r'AF_INET6?|\bconnect\(|io_uring_setup\(', events), events
        launches = re.findall(r'\bexecve(?:at)?\(', events)
        assert len(launches) == 1 and str(daimonos_binary) in events, events


def test_offline_unix_socket_sessions_inherit_admission(daimonos_binary, tmp_path):
    from test_mcp_socket import McpSocketClient

    # AF_UNIX paths have a short platform limit independent of --basetemp.
    with tempfile.TemporaryDirectory(dir="/tmp") as sockets:
        path = sockets + "/mcp.sock"
        argv = [daimonos_binary, "-w", str(tmp_path), "--offline-tools", "mcp", "--socket", path]
        log = tmp_path / "unix.trace"
        tracer = os.getenv("DAIMONOS_TEST_STRACE")
        if tracer:
            # -D keeps the tracee as our child. SIGTERM goes to the server, not the observer.
            argv = ["unshare", "-Urn", tracer, "-D", "-f", "-s", "192", "-o", str(log),
                    "-e", "trace=%network,execve,execveat,io_uring_setup", *argv]
        with subprocess.Popen(argv,
                              cwd=tmp_path, env={"HOME": str(tmp_path), "PATH": os.defpath},
                              stdout=subprocess.DEVNULL, stderr=subprocess.PIPE) as server:
            try:
                deadline = time.monotonic() + 5
                while not os.path.exists(path):
                    assert server.poll() is None, server.stderr.read()
                    assert time.monotonic() < deadline, "no Unix listener"
                    time.sleep(0.01)
                for _ in range(2):
                    client = McpSocketClient(path)
                    try:
                        client.handshake()
                        assert {t["name"] for t in client.list_tools()} == {"ls", "search", "workspace_info", "read_file"}
                        denied = client.call_tool("execute_script", {"code": "result = 1"})
                        assert json.loads(denied["content"][0]["text"])["code"] == "offline_tool_forbidden"
                        assert not client.call_tool("ls").get("isError")
                    finally:
                        client.close()
            finally:
                server.terminate()
                server.wait(timeout=5)
        if tracer:
            deadline = time.monotonic() + 5
            while "killed by SIGTERM" not in log.read_text():
                assert time.monotonic() < deadline, "missing trace through server exit"
                time.sleep(0.01)
            events = log.read_text()
            assert "AF_UNIX" in events and "accept" in events
            assert not re.search(r"AF_INET6?|\bconnect\(|io_uring_setup\(", events), events
            assert len(re.findall(r"\bexecve(?:at)?\(", events)) == 1, events
