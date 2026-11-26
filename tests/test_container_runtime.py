"""FR-10. Set DAIMONOS_TEST_IMAGE to the locally built image to run the Docker gates.

The tests never pull or publish an image. A skip does not establish AC-10.1 or AC-10.2.
"""
import json
import os
from pathlib import Path
import subprocess
import tomllib

import pytest

ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture(scope="module")
def image():
    name = os.environ.get("DAIMONOS_TEST_IMAGE")
    if not name:
        pytest.skip("build the local image and set DAIMONOS_TEST_IMAGE")
    # Resolve once so every test runs the same immutable local image.
    return subprocess.check_output(["docker", "image", "inspect", "--format={{.Id}}", name], text=True).strip()


def restricted(uid=12345):
    return ["docker", "run", "--pull=never", "--rm", "-i", "--read-only", "--network", "none", "--cap-drop", "ALL",
            "--user", f"{uid}:{uid}", "--security-opt", "no-new-privileges", "--tmpfs",
            f"/state:rw,noexec,nosuid,nodev,uid={uid},gid={uid},mode=0700"]


def test_image_version_matches_pinned_checkout(image):
    rev = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    info = json.loads(subprocess.check_output(["docker", "image", "inspect", image]))[0]["Config"]
    labels = info["Labels"]
    assert labels["org.opencontainers.image.revision"] == rev
    assert labels["org.opencontainers.image.version"] == version
    run = subprocess.run([*restricted(), image, "--version"], capture_output=True, text=True, timeout=20, check=True)
    assert run.stdout.splitlines()[:2] == [f"daimonos {version}", f"source_commit {rev}"]


def worker_args(workspace, image, mount="/review", uid=12345):
    return [*restricted(uid), "--mount", f"type=bind,src={workspace},dst={mount},readonly", image,
            "--workspace", mount, "--state-dir", "/state", "--workspace-read-only", "--ignore-config-files", "mcp"]


def mcp_exchange(argv, calls):
    init = {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "wp12", "version": "1"}}
    requests = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init},
                {"jsonrpc": "2.0", "method": "notifications/initialized"}]
    for i, (method, params) in enumerate(calls, 2):
        requests.append({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
    completed = subprocess.run(argv, input="".join(json.dumps(r) + "\n" for r in requests),
                               text=True, capture_output=True, timeout=30)
    assert completed.returncode == 0, completed.stderr
    replies = {msg["id"]: msg["result"] for line in completed.stdout.splitlines()
               if "id" in (msg := json.loads(line))}
    assert set(replies) == set(range(1, len(calls) + 2)), completed.stderr
    return replies


@pytest.mark.parametrize("mount,uid", [("/review", 12345), ("/another-location", 23456)])
def test_restricted_stdio_reads_caller_workspace(image, tmp_path, mount, uid):
    workspace = tmp_path / "mounted"
    workspace.mkdir(mode=0o755)
    fixture = workspace / "evidence.txt"
    fixture.write_text("from caller mount")
    fixture.chmod(0o444)
    replies = mcp_exchange(worker_args(workspace, image, mount, uid), [
        ("tools/list", {}),
        ("tools/call", {"name": "read_file", "arguments": {"path": fixture.name}}),
        ("tools/call", {"name": "write_file", "arguments": {"path": fixture.name, "content": "changed"}}),
    ])
    assert replies[1]["serverInfo"]["name"] == "daimonos"
    assert "read_file" in {t["name"] for t in replies[2]["tools"]}
    assert json.loads(replies[3]["content"][0]["text"])["content"] == "from caller mount"
    assert replies[4]["isError"] and "workspace_write_forbidden" in replies[4]["content"][0]["text"]
    assert list(workspace.iterdir()) == [fixture] and fixture.read_text() == "from caller mount"


def test_offline_unix_socket_in_restricted_image(image, tmp_path):
    from test_mcp_socket import McpSocketClient
    import time
    import uuid

    host_socket = tmp_path / "ipc"
    host_socket.mkdir()
    host_socket.chmod(0o700)  # owner-only directory under the private state root
    cid = "wp12-" + uuid.uuid4().hex
    # Use the invoking UID for the owner-only socket so the host test client can connect.
    uid = os.geteuid()
    if uid == 0:
        pytest.skip("the host client must use a non-root UID for the owner-only Unix socket")
    argv = [*restricted(uid), "--name", cid, "--mount", f"type=bind,src={host_socket},dst=/state/ipc", image,
            "--workspace", "/workspace", "--state-dir", "/state", "--offline-tools", "mcp", "--socket", "/state/ipc/tool.sock"]
    with subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE) as server:
        try:
            deadline = time.monotonic() + 20
            while not (host_socket / "tool.sock").is_socket():
                assert server.poll() is None, server.stderr.read()
                assert time.monotonic() < deadline
                time.sleep(0.02)
            client = McpSocketClient(str(host_socket / "tool.sock"))
            try:
                client.handshake()
                assert {t["name"] for t in client.list_tools()} == {"ls", "search", "workspace_info", "read_file"}
                assert not client.call_tool("ls").get("isError")
            finally:
                client.close()
        finally:
            subprocess.run(["docker", "rm", "-f", cid], capture_output=True, timeout=15)
            server.wait(timeout=15)


def test_default_invocation_still_serves_stdio(image):
    assert mcp_exchange(["docker", "run", "--pull=never", "--rm", "-i", "--network", "none", image],
                        [])[1]["serverInfo"]["name"] == "daimonos"


def test_unusable_private_state_refuses_before_serving(image, tmp_path):
    argv = worker_args(tmp_path, image)
    argv[argv.index("--state-dir") + 1] = "/unwritable"
    run = subprocess.run(argv, input="", text=True, capture_output=True, timeout=30)
    assert run.returncode == 2 and run.stdout == "", run.stderr
    assert json.loads(run.stderr)["error"]["reason"] == "state_unusable"


def test_model_calls_restricted_container_worker(image, daimonos_binary, tmp_path):
    from test_agent_oneshot_sessions import FakeProvider, Harness, text_reply, tool_reply

    # The evidence exists only on the tool side, so a successful answer cannot
    # come from a fallback read in the model process's workspace.
    workspace = tmp_path / "mounted"
    workspace.mkdir(mode=0o755)
    fixture = workspace / "worker.txt"
    fixture.write_text("wp12 isolated worker evidence")
    fixture.chmod(0o444)
    name = "mcp__container__read_file"
    selection = tmp_path / "selection.json"
    selection.write_text(json.dumps([name]))
    manifest = tmp_path / "workers.json"
    manifest.write_text(json.dumps({"workers": {"container": {"command": "docker", "args": worker_args(workspace, image)[1:],
                                                            "env": {"PATH": os.defpath}}}}))
    provider = FakeProvider([tool_reply(name, {"path": fixture.name}), text_reply("done")])
    try:
        host = Harness(daimonos_binary, tmp_path, provider.port)
        # The shared harness uses the literal key "test", which is also in the
        # pytest mount path. Use a distinct synthetic key, not a weaker worker guard.
        host.agent_env.write_text(host.agent_env.read_text().replace("DAIMONOS_AGENT_API_KEY=test\n",
                                                                    "DAIMONOS_AGENT_API_KEY=wp12-fake-noncredential\n"))
        state = tmp_path / "model-state"
        state.mkdir(mode=0o700)
        audit = state / "audit.jsonl"
        host.config.write_text("[analytics]\nenabled=false\n")
        assert not list(host.workspace.iterdir()) and workspace.parent == host.workspace.parent
        done = host.agent("--worker-config", str(manifest), "--output", "json", "read evidence",
                          global_args=["--tool-allowlist", str(selection), "--state-dir", str(state), "--audit-path", str(audit)])
        assert done.returncode == 0, done.stderr
        assert json.loads(done.stdout)["final_text"] == "done"
        messages = [m for m in provider.requests[-1]["messages"] if m.get("role") == "tool"]
        assert len(messages) == 1 and "wp12 isolated worker evidence" in messages[0]["content"], messages
        calls = [event for line in audit.read_text().splitlines() if (event := json.loads(line))["kind"] == "tool_call"]
        assert [e["route"] for e in calls] == ["worker"] and not list(host.workspace.iterdir())
    finally:
        provider.close()


@pytest.fixture
def offline_evidence_worker(image, tmp_path):
    """Real WP12 isolation, WP20 admission, and WP19 digests with a WP18 byte cap."""
    from test_read_only_workspace import _snapshot

    workspace = tmp_path / "evidence-only"
    workspace.mkdir(mode=0o755)
    source = workspace / "sample.txt"
    source.write_bytes(b"private-prefix\r\nvisible-page\r\nprivate-suffix\n")
    size = source.stat().st_size
    (workspace / "larger.txt").write_bytes(b"A" * (size + 1))
    for path in workspace.iterdir():
        path.chmod(0o444)
    digest = subprocess.check_output(["sha256sum", source], text=True).split()[0]
    before = _snapshot(workspace)
    argv = worker_args(workspace, image)
    argv[-1:-1] = ["--offline-tools", "--read-sha256", "--full-tool-schemas", "--no-output-spill",
                   "--confine-paths", "--no-follow-symlinks", "--tool-input-bytes", str(size)]
    yield argv, digest
    assert _snapshot(workspace) == before


def test_offline_worker_hashes_the_whole_file_within_its_byte_budget(offline_evidence_worker):
    argv, digest = offline_evidence_worker
    args = {"path": "sample.txt", "offset": 1, "limit": 1}
    results = mcp_exchange(argv, [
        ("tools/list", {}),
        ("tools/call", {"name": "read_file", "arguments": args}),
        ("tools/call", {"name": "read_file", "arguments": {**args, "path": "larger.txt"}}),
        ("tools/call", {"name": "git_read", "arguments": {"command": "resolve"}}),
    ])
    assert {t["name"] for t in results[2]["tools"]} == {"read_file", "search", "workspace_info", "ls"}
    page, refusal, forbidden = [json.loads(results[i]["content"][0]["text"]) for i in (3, 4, 5)]
    assert not results[3].get("isError") and page["content"] == "visible-page" and page["sha256"] == digest
    assert results[4]["isError"] and refusal["code"] == "tool_input_limit" and "sha256" not in refusal
    assert results[5]["isError"] and forbidden["code"] == "offline_tool_forbidden"


@pytest.mark.parametrize("wire", ["messages", "chat"])
def test_model_receives_only_paged_offline_evidence(wire, offline_evidence_worker, daimonos_binary, tmp_path):
    from test_agent_oneshot_sessions import tool_reply
    from test_capability_probe import CRED_VALUE, FINAL_SCHEMA, FINAL_TOOL, _env, _write_final_output, _write_payload, _write_profile
    from test_combined_reviewer_profile import ScriptEndpoint, _final, _worker_call

    argv, digest = offline_evidence_worker
    name = "mcp__container__read_file"
    params = {"path": "sample.txt", "offset": 1, "limit": 1}
    manifest = tmp_path / "worker.json"
    manifest.write_text(json.dumps({"workers": {"container": {"command": "docker", "args": argv[1:],
                                                            "env": {"PATH": os.defpath}}}}))
    state = tmp_path / "model-state"
    state.mkdir(mode=0o700)
    workspace = tmp_path / "empty-model-workspace"
    workspace.mkdir()
    allow = tmp_path / "allow.json"
    allow.write_text(json.dumps([name, FINAL_TOOL]))
    audit = state / "audit.jsonl"
    agent_env, env = _env(tmp_path)
    if wire == "messages":
        call = _worker_call(name)
        call["content"][0]["input"] = params
        script = [_final(), call, _final()]
    else:
        script = []
        for tool, args in [(FINAL_TOOL, {"verdict": "supported"}), (name, params), (FINAL_TOOL, {"verdict": "supported"})]:
            reply = tool_reply(tool, args)
            script.append({"choices": [{"message": reply["message"], "finish_reason": reply["finish_reason"]}],
                           "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
    endpoint = ScriptEndpoint(script)
    try:
        profile = _write_profile(tmp_path / "profile.json", endpoint.url, wire=wire,
                                 provider_label="anthropic" if wire == "messages" else "openrouter")
        run = subprocess.run([daimonos_binary, "-w", str(workspace), "--state-dir", str(state), "--workspace-read-only",
                              "--tool-allowlist", str(allow), "--full-tool-schemas", "--audit-path", str(audit), "agent",
                              "--print", "--agent-env", str(agent_env), "--worker-config", str(manifest),
                              "--model-profile", str(profile), "--final-output", str(_write_final_output(tmp_path / "final.json")),
                              "--capability-probe-payload", str(_write_payload(tmp_path / "probe.json")), "read the evidence"],
                             text=True, capture_output=True, env=env, timeout=60)
    finally:
        endpoint.close()
    assert run.returncode == 0 and json.loads(run.stdout) == {"verdict": "supported"}, run.stderr
    probe, work, finish = endpoint.requests
    assert probe["tools"] == work["tools"] == finish["tools"]
    definitions = [t.get("function", t) for t in work["tools"]]
    schemas = {t["name"]: t.get("parameters", t.get("input_schema")) for t in definitions}
    assert set(schemas) == {name, FINAL_TOOL} and schemas[FINAL_TOOL] == FINAL_SCHEMA
    assert schemas[name]["properties"]["path"]["x-daimonos-path"] is True
    evidence = json.dumps(finish["messages"])
    assert digest in evidence and "visible-page" in evidence
    assert "private-prefix" not in evidence and "private-suffix" not in evidence
    raw = audit.read_text()
    events = [json.loads(line) for line in raw.splitlines()]
    assert [(e["kind"], e.get("route")) for e in events if e["kind"] == "tool_call"] == [("tool_call", "worker")]
    assert events[-1]["kind"] == "run_end" and events[-1]["exit_code"] == 0
    assert CRED_VALUE not in raw + run.stdout + run.stderr and "visible-page" not in raw
    assert not list(workspace.iterdir())
