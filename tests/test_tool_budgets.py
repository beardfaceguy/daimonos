"""WP18 FR-19 process-level acceptance tests: cooperative tool budgets.

Spawns a real daimonos MCP process (conftest ``daimonos_factory``) and drives
the public tool interface. Budgets are default-off; every test that asserts a
limit passes the CLI option explicitly. Context:
docs/adr/031-cooperative-tool-budgets.md and the reviewer-runtime
INTERFACES.md row "WP18, FR-19".
"""

import json


def _text(result):
    return result["content"][0]["text"]


def test_input_bytes_limit_refuses_oversized_read(daimonos_factory, tmp_path):
    """--tool-input-bytes N: a read whose logical input exceeds N is refused
    with the reviewed ``tool_input_limit`` code and a budget naming the
    configured value and the consumed value. No partial content is returned.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "big.txt").write_text("x" * 500)
    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])

    result = client.call_tool("read_file", {"path": "big.txt"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_input_limit", data
    assert data["configured"] == 100, data
    assert data["consumed"] >= 500, data
    assert "content" not in data, "no partial result on a limit hit"


def test_input_bytes_default_off_reads_normally(daimonos_factory, tmp_path):
    """Regression: with no budget option, an ordinary read is unchanged."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "big.txt").write_text("x" * 500)
    client = daimonos_factory(ws)
    result = client.call_tool("read_file", {"path": "big.txt"})
    data = json.loads(_text(result))
    assert data["content"] == "x" * 500


import os
import time


def _survivors(needle):
    """PIDs whose /proc cmdline contains `needle` — the process-table method of
    tests/test_worker_isolation.py::_worker_survivors, used here to prove that
    neither the exec child nor its grandchild outlives a budget stop (FR-19
    requirement 3)."""
    out = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open("/proc/%s/cmdline" % pid, "rb") as f:
                cmd = f.read().replace(b"\x00", b" ").decode("utf-8", "replace")
        except OSError:
            continue
        if needle in cmd:
            out.append(pid)
    return out


def _poll_gone(needle, bound_s):
    deadline = time.monotonic() + bound_s
    while time.monotonic() < deadline:
        if not _survivors(needle):
            return True
        time.sleep(0.05)
    return not _survivors(needle)


def test_timeout_stops_exec_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
    """--tool-timeout-ms N: an exec that outlives the deadline is stopped with
    the reviewed ``tool_timeout`` code, and neither the child shell nor the
    grandchild it backgrounds survives (FR-19 requirement 3; stopped work, not
    only a stopped wait — the managed process group is TERM/KILL reaped via
    ManagedChild::terminate).

    Timing (AC-19.1, req 9): the deadline is 1000 ms — a limit of one second or
    less so a slower CI runner still stops promptly. The call is asserted to
    return within 8 s: generous for a loaded shared runner (expected stop is
    ~1-3 s), yet well below the 15 s the shells would otherwise sleep, so a
    pass proves the stop was the budget deadline, not the processes exiting on
    their own.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    gc_marker = f"wp18gc-{os.getpid()}-{time.time_ns()}"
    parent_marker = f"wp18parent-{os.getpid()}-{time.time_ns()}"
    # The shell daimonos runs (sh -c <line>) backgrounds a grandchild shell
    # whose argv carries gc_marker, then itself sleeps far past the deadline.
    # Both stay in the exec's process group, so a group reap removes both.
    line = (
        f"sh -c 'sleep 15 # {gc_marker}' & "
        f"sleep 15 # {parent_marker}"
    )
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1000"])

    start = time.monotonic()
    result = client.call_tool("exec", {"command": line})
    elapsed = time.monotonic() - start

    assert elapsed < 8, f"budget did not stop exec promptly: {elapsed:.1f}s"
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_timeout", data
    assert "exit" not in data, "no partial exec result on a deadline hit"

    assert _poll_gone(parent_marker, 5.0), "exec child survived the budget stop"
    assert _poll_gone(gc_marker, 5.0), "exec grandchild survived the budget stop"


def test_max_entries_limit_refuses_oversized_ls(daimonos_factory, tmp_path):
    """--tool-max-entries N: an ls that walks more than N directory entries is
    refused with the reviewed ``tool_input_limit`` code (integrator D56: the
    entry ceiling reuses the byte code), a budget naming the configured value
    and the consumed entry count, and no partial listing. Off by default.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(10):
        (ws / f"f{i}.txt").write_text("x")
    client = daimonos_factory(ws, args=["--tool-max-entries", "3"])

    result = client.call_tool("ls", {"path": "."})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_input_limit", data
    assert data["configured"] == 3, data
    assert data["consumed"] >= 10, data
    assert "entries" not in data, "no partial listing on an entry-limit hit"


def test_max_entries_default_off_lists_normally(daimonos_factory, tmp_path):
    """Regression: with no budget option, an ordinary ls is unchanged."""
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(10):
        (ws / f"f{i}.txt").write_text("x")
    client = daimonos_factory(ws)
    result = client.call_tool("ls", {"path": "."})
    data = json.loads(_text(result))
    assert len(data["entries"]) == 10, data


def test_input_bytes_meters_content_search_filesystem_path(daimonos_factory, tmp_path):
    """--tool-input-bytes N on the FILESYSTEM search path (search mode=content
    -> grep): the logical input the search returns is charged, so a result that
    exceeds N is refused with ``tool_input_limit`` and no partial matches
    (FR-19 req 4; the grep path reads the filesystem, not the index).
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(6):
        (ws / f"hit_{i}.txt").write_text("needle here\n")
    client = daimonos_factory(ws, args=["--tool-input-bytes", "40"])

    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_input_limit", data
    assert data["configured"] == 40, data
    assert "matches" not in data, "no partial matches on a limit hit"


def test_input_bytes_refuses_index_search(daimonos_factory, tmp_path):
    """F1 (M1): the INDEX search path (search mode=files -> trigram index)
    reads the cached index, not scannable file content, so its logical input
    cannot be metered. Under --tool-input-bytes the index-answered search is
    REFUSED with a named error rather than charged "bytes returned" (ADR-031
    allows refusing an enabled-but-unenforceable budget; the owner choice is
    stated in the handoff). No partial results.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(8):
        (ws / f"wp18idx_{i}.rs").write_text("x")
    client = daimonos_factory(ws, args=["--tool-input-bytes", "30"])

    result = client.call_tool("search", {"pattern": "wp18idx", "mode": "files"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_budget_unenforceable", data
    assert "results" not in data, "no partial index results on a refusal"


# --- Composites share one budget (FR-19 requirement 2) ---

_TWO_READS = (
    "def main():\n"
    "    read_file('big.txt')\n"
    "    read_file('big.txt')\n"
    "    return 'both'\n"
    "result = main()\n"
)


def test_script_children_share_parent_budget(daimonos_factory, tmp_path):
    """FR-19 req 2: a composite's children inherit the parent call's remaining
    allowance, they do NOT each get a fresh top-level budget. Two reads of a
    60-byte file under --tool-input-bytes 100: the first (60) fits, the second
    (cumulative 120) trips the shared ceiling. This test FAILS if an
    execute_script child is given its own new budget (both reads would fit).
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "big.txt").write_text("y" * 60)
    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])

    result = client.call_tool("execute_script", {"code": _TWO_READS})
    assert result.get("isError") is True, result
    assert "tool_input_limit" in _text(result), _text(result)


def test_script_single_read_within_budget_succeeds(daimonos_factory, tmp_path):
    """Control: one 60-byte read under --tool-input-bytes 100 inside a script
    succeeds, so the failure above is the shared second read, not read_file
    being charged wrong."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "big.txt").write_text("y" * 60)
    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
    code = "def main():\n    read_file('big.txt')\n    return 'ok'\nresult = main()\n"
    result = client.call_tool("execute_script", {"code": code})
    assert result.get("isError") is not True, _text(result)
    assert json.loads(_text(result))["result"] == "ok"


def test_batch_children_share_parent_budget(daimonos_factory, tmp_path):
    """FR-19 req 2: a batch's ops inherit the one parent budget. Two reads of a
    60-byte file under --tool-input-bytes 100: op 1 fits, op 2 (cumulative 120)
    trips the shared ceiling. FAILS if a batch op gets its own new budget."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "big.txt").write_text("y" * 60)
    client = daimonos_factory(ws, args=["--tool-input-bytes", "100"])
    result = client.call_tool("batch", {"ops": [
        {"tool": "read_file", "arguments": {"path": "big.txt"}},
        {"tool": "read_file", "arguments": {"path": "big.txt"}},
    ]})
    content = json.loads(_text(result))
    assert content[0]["ok"] is True, content
    assert content[1]["ok"] is False, content
    assert "tool_input_limit" in json.dumps(content[1]), content


# --- Agent `--print` frontend: audit (req 5), --output json (req 11), req 1 ---
#
# A local, self-contained fake OpenAI-compatible endpoint (no network, no real
# credential) drives a real `daimonos agent --print` process, mirroring
# tests/test_run_audit.py. A budget option is passed as a global flag so the
# agent frontend honors it; a limit hit must be a tool failure INSIDE the run
# (the run still succeeds and, under --output json, still prints one document),
# and the WP08 audit must record the named code.

import os as _os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

_FAKE_KEY = "synthetic-not-a-real-key-WP18-5fZ9"
_DONE = "WP18_AGENT_DONE_MARKER"


class _FakeProvider:
    def __init__(self, script):
        self._script = list(script)
        self._step = 0
        self.received_tool_results = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                payload = json.dumps({"data": [{"id": "test/model",
                    "context_length": 100000, "supported_parameters": ["tools"],
                    "pricing": {"prompt": "0", "completion": "0"}}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                tool_msgs = [m for m in body["messages"] if m.get("role") == "tool"]
                if tool_msgs:
                    owner.received_tool_results.append(json.dumps(tool_msgs))
                step = owner._script[min(owner._step, len(owner._script) - 1)]
                owner._step += 1
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

                def chunk(delta, reason=None):
                    frame = {"choices": [{"index": 0, "delta": delta,
                        "finish_reason": reason}],
                        "usage": {"prompt_tokens": 42, "completion_tokens": 7}}
                    self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                    self.wfile.flush()

                try:
                    if step[0] == "call":
                        _, name, arguments = step
                        chunk({"role": "assistant", "content": "", "tool_calls": [{
                            "index": 0, "id": f"call-{owner._step}", "type": "function",
                            "function": {"name": name,
                                         "arguments": json.dumps(arguments)}}]},
                              "tool_calls")
                    else:
                        chunk({"role": "assistant", "content": step[1]}, "stop")
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self):
        return self.server.server_port

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


def _agent_env_file(tmp_path, provider, allowed="read_file"):
    path = tmp_path / "agent.env"
    path.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.port}/v1",
        f"DAIMONOS_AGENT_API_KEY={_FAKE_KEY}",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        f"DAIMONOS_AGENT_ALLOWED_COMMANDS={allowed}",
        "",
    ]))
    return path


def _isolated_env(tmp_path):
    stash = tmp_path / "home"
    stash.mkdir(exist_ok=True)
    (tmp_path / "tmp").mkdir(exist_ok=True)
    env = {k: v for k, v in _os.environ.items() if not k.startswith("DAIMONOS_")}
    env.update({
        "HOME": str(stash),
        "XDG_STATE_HOME": str(stash / "state"),
        "XDG_DATA_HOME": str(stash / "data"),
        "XDG_CONFIG_HOME": str(stash / "config"),
        "TMPDIR": str(tmp_path / "tmp"),
        "DAIMONOS_KGL_AUTOINDEX": "0",
    })
    return env


def test_agent_print_json_budget_hit_is_tool_failure_and_is_audited(daimonos_binary, tmp_path):
    """req 1 (agent --print honors the budget), req 11 (a limit hit under
    --output json is a tool failure INSIDE the run; the run still prints its one
    JSON document and exits 0) and req 5 (the WP08 audit records the hit with
    its named code). The model calls read_file on a 500-byte file under
    --tool-input-bytes 100, gets a tool_input_limit, then ends normally.
    """
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "big.txt").write_text("z" * 500)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
    try:
        env = _isolated_env(tmp_path)
        agent_env = _agent_env_file(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "--tool-input-bytes", "100",
               "agent", "--print", "--output", "json",
               "--agent-env", str(agent_env), "do the read"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=60, env=env)
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stdout + completed.stderr
    # req 11: exactly one JSON document on stdout.
    doc = json.loads(completed.stdout)
    assert isinstance(doc, dict), completed.stdout
    # The tool result handed back to the model carried the named code: the
    # limit hit was a tool failure inside the run, not a run abort.
    joined = " ".join(provider.received_tool_results)
    assert "tool_input_limit" in joined, joined
    # req 5: the audit recorded the hit with its named code.
    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
    assert any("tool_input_limit" in json.dumps(ev) for ev in events), events


# --- git_read deadline + process-group reap (FR-19 req 3; slice 06) ---

import shutil


def _init_repo(ws):
    """A minimal real git repo for git_read to open, built with the real git on
    PATH before any fake git is installed."""
    real = shutil.which("git")
    assert real, "a real git is required to build the fixture repo"

    def g(*a):
        subprocess.run([real, *a], cwd=str(ws), check=True,
                       capture_output=True, env={**_os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
                                                  "GIT_CONFIG_SYSTEM": "/dev/null"})
    g("init", "-q")
    g("config", "user.email", "wp18@example.invalid")
    g("config", "user.name", "WP18")
    (ws / "a.txt").write_text("hello\n")
    g("add", "a.txt")
    g("commit", "-q", "-m", "seed")
    return real


def _install_fake_git(bindir, real_git, parent_marker, gc_marker):
    """A `git` shim that passes repo discovery through to the real git but, once
    git_read sets GIT_DIR for an operation, backgrounds a grandchild and sleeps
    far past the deadline. Both descendants carry a unique marker in their argv
    and stay in the managed child's process group, so a group reap removes them.
    """
    bindir.mkdir(parents=True, exist_ok=True)
    shim = bindir / "git"
    shim.write_text(
        "#!/bin/sh\n"
        "if [ -n \"$GIT_DIR\" ]; then\n"
        f"  sh -c 'sleep 12 # {gc_marker}' &\n"
        f"  exec sh -c 'sleep 12 # {parent_marker}'\n"
        "fi\n"
        f"exec {real_git} \"$@\"\n"
    )
    shim.chmod(0o755)


def test_timeout_stops_git_read_and_reaps_child_and_grandchild(daimonos_factory, tmp_path):
    """--tool-timeout-ms N: a git_read whose git subprocess outlives the
    deadline is stopped with the reviewed ``tool_timeout`` code, and neither the
    git child nor the grandchild it backgrounds survives (FR-19 req 3 names
    git_read for a reap proof; stopped work, not only a stopped wait — the whole
    process group is TERM/KILL reaped via ManagedChild::terminate, the method of
    tests/test_worker_isolation.py::test_r6_stalled_worker_is_force_reaped).

    git_read is dispatched directly (not via ops::dispatch), so before this
    slice a standalone git_read got no budget at all; the deadline is threaded
    into managed_process::run via a task-local so the git child is actually
    reaped, not merely abandoned.

    Timing (AC-19.1, req 9): the deadline is 1500 ms — one and a half seconds so
    a slower CI runner still stops promptly. The call is asserted to return
    within 8 s (expected stop ~1.5-3 s) yet well below the 12 s the shims would
    otherwise sleep, so a pass proves the stop was the budget deadline, not the
    shims exiting on their own.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    real_git = _init_repo(ws)
    bindir = tmp_path / "bin"
    parent_marker = f"wp18git-{_os.getpid()}-{time.time_ns()}"
    gc_marker = f"wp18gitgc-{_os.getpid()}-{time.time_ns()}"
    _install_fake_git(bindir, real_git, parent_marker, gc_marker)

    env = {"PATH": f"{bindir}{_os.pathsep}{_os.environ.get('PATH','')}",
           "DAIMONOS_KGL_AUTOINDEX": "0"}
    client = daimonos_factory(ws, env=env, args=["--tool-timeout-ms", "1500"])

    start = time.monotonic()
    result = client.call_tool("git_read", {"command": "log"})
    elapsed = time.monotonic() - start

    assert elapsed < 8, f"budget did not stop git_read promptly: {elapsed:.1f}s"
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_timeout", data
    assert "entries" not in data, "no partial git_read result on a deadline hit"

    assert _poll_gone(parent_marker, 5.0), "git_read child survived the budget stop"
    assert _poll_gone(gc_marker, 5.0), "git_read grandchild survived the budget stop"


# --- Default off: every metered tool is unchanged with no budget (req 8) ---
#
# read_file and ls already have default-off regressions above. These cover the
# remaining tools FR-19 req 8 names: exec, search, git_read. With none of the
# three options, the new code paths (budget install, the with_budget_deadline
# wrapper around managed_process::run) must be exact no-ops.


def test_exec_default_off_runs_normally(daimonos_factory, tmp_path):
    """Regression (req 8): with no budget option an ordinary exec is unchanged
    — it completes and returns its real exit, not a tool_timeout."""
    ws = tmp_path / "ws"
    ws.mkdir()
    client = daimonos_factory(ws)
    result = client.call_tool("exec", {"command": "printf wp18ok"})
    assert result.get("isError") is not True, _text(result)
    data = json.loads(_text(result))
    assert data["exit"] == 0, data
    assert "wp18ok" in json.dumps(data), data


def test_search_default_off_returns_all_matches(daimonos_factory, tmp_path):
    """Regression (req 8): with no budget option, a content search returns its
    full result (no tool_input_limit)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(6):
        (ws / f"hit_{i}.txt").write_text("needle here\n")
    client = daimonos_factory(ws)
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert result.get("isError") is not True, _text(result)
    assert "tool_input_limit" not in _text(result), _text(result)


def test_git_read_default_off_runs_normally(daimonos_factory, tmp_path):
    """Regression (req 8): with no budget option a git_read is unchanged — the
    with_budget_deadline(None, ..) wrapper around the git subprocess is an exact
    no-op and the real git log is returned, not a tool_timeout."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _init_repo(ws)
    client = daimonos_factory(ws)
    result = client.call_tool("git_read", {"command": "log"})
    assert result.get("isError") is not True, _text(result)
    assert "tool_timeout" not in _text(result), _text(result)
    assert "entries" in _text(result), _text(result)


# --- Combinations: the budget composes with every other run option (req 6) ---
#
# Each test pairs a budget option with one other run-shaping option and asserts
# the budget still fires with its reviewed code. Output truncation stays a
# SEPARATE mechanism: the last test proves a result bounded by --no-output-spill
# is not reported as a budget limit failure.


def _big(ws, n=500):
    (ws / "big.txt").write_text("x" * n)


def test_input_bytes_composes_with_tool_allowlist(daimonos_factory, tmp_path):
    """req 6: --tool-input-bytes together with --tool-allowlist. read_file is on
    the allowlist, so admission passes; the byte budget still trips with
    tool_input_limit (the allowlist and the budget are independent gates)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _big(ws)
    allowlist = tmp_path / "allow.json"
    allowlist.write_text(json.dumps(["read_file"]))
    client = daimonos_factory(ws, args=["--tool-allowlist", str(allowlist),
                                        "--tool-input-bytes", "100"])
    result = client.call_tool("read_file", {"path": "big.txt"})
    assert result.get("isError") is True, result
    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)


def test_input_bytes_composes_with_workspace_read_only(daimonos_factory, tmp_path):
    """req 6: --tool-input-bytes together with --workspace-read-only. A read is
    allowed under read-only, and the byte budget still trips."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _big(ws)
    client = daimonos_factory(ws, args=["--workspace-read-only",
                                        "--tool-input-bytes", "100"])
    result = client.call_tool("read_file", {"path": "big.txt"})
    assert result.get("isError") is True, result
    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)


def test_input_bytes_composes_with_confine_paths_and_no_follow(daimonos_factory, tmp_path):
    """req 6: --tool-input-bytes together with --confine-paths and
    --no-follow-symlinks. The read is inside the confined workspace and the byte
    budget still trips with tool_input_limit."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _big(ws)
    client = daimonos_factory(ws, args=["--confine-paths", "--no-follow-symlinks",
                                        "--tool-input-bytes", "100"])
    result = client.call_tool("read_file", {"path": "big.txt"})
    assert result.get("isError") is True, result
    assert json.loads(_text(result))["code"] == "tool_input_limit", _text(result)


def test_no_output_spill_truncation_is_not_a_budget_limit(daimonos_factory, tmp_path):
    """req 6: output truncation stays a mechanism distinct from the input
    budget. A large read under --no-output-spill is bounded by the output rules,
    not failed; with a generous --tool-input-bytes that the logical input fits
    under, the result must NOT carry a budget code. Truncation != limit hit."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _big(ws, n=300_000)
    client = daimonos_factory(ws, args=["--no-output-spill",
                                        "--tool-input-bytes", "10000000"])
    result = client.call_tool("read_file", {"path": "big.txt"})
    body = _text(result)
    assert "tool_input_limit" not in body, body
    assert "tool_timeout" not in body, body


# --- Workers (WP10, FR-19 req 7): a budgeted worker route is refused ---
#
# The cooperative budget installs at THIS process's dispatch seams, but a worker
# routes tool calls into a separate process the parent budget never sees. So a
# budget option cannot be enforced across the worker boundary; rather than
# silently ignore it (a frontend-rule violation), the run is refused at startup
# with run_configuration_invalid / exit 2 (D-WP18-5; ADR-031 refuse
# enabled-but-unenforceable; INTERFACES worker rule). Ordinary worker mode with
# no budget option is unchanged.

_WORKER_REFUSAL = "tool_budget_worker_unenforceable"


def _min_worker_config(tmp_path):
    wc = tmp_path / "wc.json"
    wc.write_text(json.dumps({"workers": {"w": {
        "command": "/bin/true", "args": [],
        "env": {"PATH": _os.environ.get("PATH", "/usr/bin:/bin")}}}}))
    return wc


def _run_agent_worker(daimonos_binary, tmp_path, provider, budget_args):
    env = _isolated_env(tmp_path)
    agent_env = _agent_env_file(tmp_path, provider)
    ws = tmp_path / "ws"
    ws.mkdir(exist_ok=True)
    wc = _min_worker_config(tmp_path)
    cmd = [daimonos_binary, "--workspace", str(ws), *budget_args,
           "agent", "--print", "--worker-config", str(wc),
           "--agent-env", str(agent_env), "do it"]
    return subprocess.run(cmd, capture_output=True, text=True, timeout=60, env=env)


def test_budgeted_worker_route_is_refused_at_startup(daimonos_binary, tmp_path):
    """req 7: --tool-timeout-ms together with --worker-config is refused before
    any worker launches, with the reviewed run_configuration_invalid code and
    exit 2 and a reason naming the unenforceable budget."""
    provider = _FakeProvider([("end", _DONE)])
    try:
        completed = _run_agent_worker(daimonos_binary, tmp_path, provider,
                                      ["--tool-input-bytes", "1000"])
    finally:
        provider.close()
    blob = completed.stdout + completed.stderr
    assert completed.returncode == 2, blob
    assert "run_configuration_invalid" in blob, blob
    assert _WORKER_REFUSAL in blob, blob


def test_unbudgeted_worker_route_is_not_refused_for_budget(daimonos_binary, tmp_path):
    """req 7 control: with no budget option, worker mode is unchanged — it runs
    its own WP10 path (here the trivial /bin/true worker is simply unavailable);
    whatever its outcome, it is NOT the budget refusal."""
    provider = _FakeProvider([("end", _DONE)])
    try:
        completed = _run_agent_worker(daimonos_binary, tmp_path, provider, [])
    finally:
        provider.close()
    blob = completed.stdout + completed.stderr
    assert _WORKER_REFUSAL not in blob, blob


# ======================================================================
# FIX run r4 (defects M1-M5). Each test targets one integrator measurement.
# ======================================================================


def test_input_bytes_charges_bytes_scanned_not_returned(daimonos_factory, tmp_path):
    """F1 (M1): a content search charges the bytes it SCANS, not the bytes it
    returns. 40 files of 100 KB without the word plus one with it, under
    --tool-input-bytes 2000: the scan must stop and the call must fail with
    ``tool_input_limit`` even though the returned match set is tiny. ``consumed``
    exceeds the limit (the scan was stopped mid-stream) and stays below the
    total bytes on disk (it stopped early). No matches on a limit hit.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    blob = "x" * (100 * 1024)
    for i in range(40):
        (ws / f"pad_{i:02}.txt").write_text(blob)
    (ws / "zzz_match.txt").write_text("needle\n")
    client = daimonos_factory(ws, args=["--tool-input-bytes", "2000"])

    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_input_limit", data
    assert data["configured"] == 2000, data
    assert data["consumed"] > 2000, data
    assert data["consumed"] <= 41 * 100 * 1024, data
    assert "matches" not in data, "no partial matches on a limit hit"


def test_input_bytes_content_search_below_limit_passes(daimonos_factory, tmp_path):
    """F1 control: a content scan whose total scanned bytes stay under the
    ceiling returns its matches normally (so the failure above is the ceiling,
    not search being broken)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "a.txt").write_text("needle here\n")  # ~12 bytes scanned
    client = daimonos_factory(ws, args=["--tool-input-bytes", "100000"])
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert result.get("isError") is not True, _text(result)
    data = json.loads(_text(result))
    assert len(data["matches"]) == 1, data


def test_timeout_read_file_returns_timeout(daimonos_factory, tmp_path):
    """F2 (M2): read_file checks the deadline before building its result; with a
    1 ms deadline (already elapsed by the time the native reader runs) it
    returns ``tool_timeout``, never a late success with content."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "big.txt").write_text("x" * (100 * 1024))
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
    result = client.call_tool("read_file", {"path": "big.txt"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_timeout", data
    assert "content" not in data, "no late content on a deadline hit"


def test_timeout_ls_returns_timeout(daimonos_factory, tmp_path):
    """F2 (M2): the ls walk checks the deadline before building its result."""
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(20):
        (ws / f"f{i}.txt").write_text("x")
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
    result = client.call_tool("ls", {"path": "."})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_timeout", data
    assert "entries" not in data, "no late listing on a deadline hit"


def test_timeout_content_search_returns_timeout(daimonos_factory, tmp_path):
    """F2 (M2): the content-search blocking worker itself checks the deadline
    (passed into the worker), so a 1 ms deadline returns ``tool_timeout``, not a
    late success."""
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(20):
        (ws / f"f{i}.txt").write_text("needle\n")
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_timeout", data
    assert "matches" not in data, "no late matches on a deadline hit"


def test_timeout_file_search_returns_timeout(daimonos_factory, tmp_path):
    """F2 (M2): the index (files-mode) search checks the deadline before the
    result is built, so a 1 ms deadline returns ``tool_timeout``."""
    ws = tmp_path / "ws"
    ws.mkdir()
    for i in range(20):
        (ws / f"wp18f_{i}.rs").write_text("x")
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
    result = client.call_tool("search", {"pattern": "wp18f", "mode": "files"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data["code"] == "tool_timeout", data


def test_ac_19_1_large_tree_search_times_out_within_bound(daimonos_factory, tmp_path):
    """AC-19.1 / F2: a content search on a large tree (10000 files x ~1 KB =
    ~10 MB; a full walk+scan of this many files clearly exceeds the limit on any
    host in this environment, where opening 10000 files alone costs well over
    100 ms) with a 100 ms limit returns ``tool_timeout`` within a stated upper
    bound (8 s, far below the time a full scan would take), proving the deadline
    STOPPED the work rather than the scan finishing on its own. The control with
    no option returns success. Kept under ~10 s including setup.
    """
    ws = tmp_path / "ws"
    ws.mkdir()
    blob = "lorem ipsum dolor sit amet " * 38  # ~1 KB, no match
    for i in range(10000):
        (ws / f"f{i:05}.txt").write_text(blob)

    client = daimonos_factory(ws, args=["--tool-timeout-ms", "100"])
    start = time.monotonic()
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    elapsed = time.monotonic() - start
    assert elapsed < 8, f"budget did not stop the search promptly: {elapsed:.1f}s"
    assert result.get("isError") is True, result
    assert json.loads(_text(result))["code"] == "tool_timeout", _text(result)

    control = daimonos_factory(ws)
    r2 = control.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert r2.get("isError") is not True, _text(r2)
    assert "tool_timeout" not in _text(r2), _text(r2)


# --- F3: plugin/managed-process tools (M3 curl, M4 git) ---


def _slow_http(delay_s):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_GET(self):
            time.sleep(delay_s)
            body = b"wp18-slow-body"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()
    return server, t


def test_timeout_stops_curl_plugin(daimonos_factory, tmp_path):
    """F3 (M3): the curl plugin starts a subprocess; a --tool-timeout-ms 700
    deadline stops and reaps it, returning ``tool_timeout`` in under 3 s against
    a server that would not answer for 4 s, and leaving no curl child alive."""
    ws = tmp_path / "ws"
    ws.mkdir()
    server, _t = _slow_http(4.0)
    port = server.server_port
    url = f"http://127.0.0.1:{port}/"
    needle = f"wp18curl-{_os.getpid()}-{time.time_ns()}"
    try:
        client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
        start = time.monotonic()
        # The url carries a unique marker so _survivors can spot a leaked child.
        result = client.call_tool("curl", {"url": url + needle})
        elapsed = time.monotonic() - start
    finally:
        server.shutdown()
        server.server_close()
    assert elapsed < 3, f"curl ran past the deadline: {elapsed:.1f}s"
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data.get("code") == "tool_timeout", data
    assert _poll_gone(needle, 3.0), "curl child survived the budget stop"


def test_timeout_stops_git_plugin(daimonos_factory, tmp_path):
    """F4-of-M4 (timeout half): the `git` plugin (a subprocess) is stopped by
    --tool-timeout-ms 1 with ``tool_timeout``, not a success."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _init_repo(ws)
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "1"])
    result = client.call_tool("git", {"command": "status"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data.get("code") == "tool_timeout", data


def test_input_bytes_refuses_git_plugin(daimonos_factory, tmp_path):
    """F3 (M4 input half): the `git` plugin reads no meterable logical input, so
    under --tool-input-bytes it is REFUSED with ``tool_budget_unenforceable``
    rather than passing uncounted (ADR-031)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _init_repo(ws)
    client = daimonos_factory(ws, args=["--tool-input-bytes", "10"])
    result = client.call_tool("git", {"command": "status"})
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data.get("code") == "tool_budget_unenforceable", data


def test_git_plugin_default_off_runs_normally(daimonos_factory, tmp_path):
    """Regression: with no budget option the `git` plugin is unchanged."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _init_repo(ws)
    client = daimonos_factory(ws)
    result = client.call_tool("git", {"command": "status"})
    assert result.get("isError") is not True, _text(result)
    assert "tool_timeout" not in _text(result), _text(result)
    assert "tool_budget_unenforceable" not in _text(result), _text(result)


def test_budgeted_pipeline_is_refused(daimonos_factory, tmp_path):
    """F3 (pipeline): tool_pipeline runs external plugin stages whose remaining
    deadline/input cannot be enforced stage-by-stage, so rather than leave it
    "not cooperative" it is REFUSED with ``tool_budget_unenforceable`` whenever
    any budget option is set (ADR-031 allows refusing the pipeline when
    --tool-timeout-ms is set; the same applies to input/entry). The call returns
    promptly and starts no stage process."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _init_repo(ws)
    client = daimonos_factory(ws, args=["--tool-timeout-ms", "700"])
    start = time.monotonic()
    result = client.call_tool("tool_pipeline",
                              {"tool_id": "git", "stages": ["status"]})
    elapsed = time.monotonic() - start
    assert elapsed < 3, f"refusal was not prompt: {elapsed:.1f}s"
    assert result.get("isError") is True, result
    data = json.loads(_text(result))
    assert data.get("code") == "tool_budget_unenforceable", data


def test_pipeline_default_off_not_refused(daimonos_factory, tmp_path):
    """Regression: with no budget option a tool_pipeline is not budget-refused."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _init_repo(ws)
    client = daimonos_factory(ws)
    result = client.call_tool("tool_pipeline",
                              {"tool_id": "git", "stages": ["status"]})
    assert "tool_budget_unenforceable" not in _text(result), _text(result)


# --- F5: the audit limit_hit event, asserted by kind and fields ---


def test_budget_hit_writes_limit_hit_audit_event(daimonos_binary, tmp_path):
    """F5 (M5): a budget limit hit in a run with --audit-path writes the
    reserved ``limit_hit`` audit event via record_limit_hit, carrying the limit
    name, the tool, the configured value, and the consumed value. Asserted by
    the event ``kind`` and its fields, not by a substring search for a code."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "big.txt").write_text("z" * 500)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    audit_path = tmp_path / "state" / "audit" / "run.jsonl"
    provider = _FakeProvider([("call", "read_file", {"path": "big.txt"}), ("end", _DONE)])
    try:
        env = _isolated_env(tmp_path)
        agent_env = _agent_env_file(tmp_path, provider)
        cmd = [daimonos_binary, "--workspace", str(workspace),
               "--state-dir", str(state), "--audit-path", str(audit_path),
               "--tool-input-bytes", "100",
               "agent", "--print", "--output", "json",
               "--agent-env", str(agent_env), "do the read"]
        completed = subprocess.run(cmd, capture_output=True, text=True,
                                   timeout=60, env=env)
    finally:
        provider.close()
    assert completed.returncode == 0, completed.stdout + completed.stderr
    events = [json.loads(l) for l in audit_path.read_text().splitlines() if l.strip()]
    hits = [ev for ev in events if ev.get("kind") == "limit_hit"]
    assert hits, f"no limit_hit event: {[e.get('kind') for e in events]}"
    hit = hits[0]
    assert hit["tool"] == "read_file", hit
    assert hit["limit"], hit
    assert hit["configured"] == 100, hit
    assert hit["consumed"] >= 500, hit
