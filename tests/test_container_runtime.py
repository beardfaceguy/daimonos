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


def restricted():
    return ["docker", "run", "--pull=never", "--rm", "-i", "--read-only", "--network", "none", "--cap-drop", "ALL",
            "--user", "12345:12345", "--security-opt", "no-new-privileges", "--tmpfs",
            "/state:rw,noexec,nosuid,nodev,uid=12345,gid=12345,mode=0700"]


def test_image_version_matches_pinned_checkout(image):
    rev = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    info = json.loads(subprocess.check_output(["docker", "image", "inspect", image]))[0]["Config"]
    labels = info["Labels"]
    assert labels["org.opencontainers.image.revision"] == rev
    assert labels["org.opencontainers.image.version"] == version
    run = subprocess.run([*restricted(), image, "--version"], capture_output=True, text=True, timeout=20, check=True)
    assert run.stdout.splitlines()[:2] == [f"daimonos {version}", f"source_commit {rev}"]


def worker_args(workspace, image):
    return [*restricted(), "--mount", f"type=bind,src={workspace},dst=/review,readonly", image,
            "--workspace", "/review", "--state-dir", "/state", "--workspace-read-only", "--ignore-config-files", "mcp"]


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


def test_restricted_stdio_reads_caller_workspace(image, tmp_path):
    workspace = tmp_path / "mounted"
    workspace.mkdir(mode=0o755)
    fixture = workspace / "evidence.txt"
    fixture.write_text("from caller mount")
    fixture.chmod(0o444)
    replies = mcp_exchange(worker_args(workspace, image), [
        ("tools/list", {}),
        ("tools/call", {"name": "read_file", "arguments": {"path": fixture.name}}),
        ("tools/call", {"name": "write_file", "arguments": {"path": fixture.name, "content": "changed"}}),
    ])
    assert replies[1]["serverInfo"]["name"] == "daimonos"
    assert "read_file" in {t["name"] for t in replies[2]["tools"]}
    assert json.loads(replies[3]["content"][0]["text"])["content"] == "from caller mount"
    assert replies[4]["isError"] and "workspace_write_forbidden" in replies[4]["content"][0]["text"]
    assert list(workspace.iterdir()) == [fixture] and fixture.read_text() == "from caller mount"


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
        done = host.agent("--worker-config", str(manifest), "--output", "json", "read evidence",
                          global_args=["--tool-allowlist", str(selection)])
        assert done.returncode == 0, done.stderr
        assert json.loads(done.stdout)["final_text"] == "done"
        messages = [m for m in provider.requests[-1]["messages"] if m.get("role") == "tool"]
        assert len(messages) == 1 and "wp12 isolated worker evidence" in messages[0]["content"], messages
    finally:
        provider.close()
