"""FR-17 / ADR-031: hashes cover stored bytes, not the visible page."""
import json
import subprocess

import pytest


def digest(path):
    return subprocess.check_output(["sha256sum", path], text=True).split()[0]


def payload(client, name, args):
    result = client.call_tool(name, args)
    assert not result.get("isError"), result
    return json.loads(result["content"][0]["text"])


def test_read_hashes_all_bytes_not_the_page(daimonos_factory, tmp_path):
    path = tmp_path / "fixture"
    path.write_bytes(b"alpha\r\nbeta\r\nomega")
    client = daimonos_factory(tmp_path, args=["--read-sha256"], env={"HOME": str(tmp_path)})
    page = payload(client, "read_file", {"path": path.name, "offset": 1, "limit": 1})
    assert page["content"] == "beta" and page["sha256"] == digest(path)


def test_filesystem_binary_dedup_budget_and_legacy(daimonos_factory, tmp_path):
    workspace = tmp_path / "repo"
    workspace.mkdir()
    raw = workspace / "binary"
    raw.write_bytes(b"withhold this\x00\xff\xfe")
    txt = workspace / "text"
    txt.write_text("text")
    for flags in ([], ["--confine-paths", "--state-dir", str(tmp_path / "state")]):
        (tmp_path / "state").mkdir(mode=0o700, exist_ok=True)
        client = daimonos_factory(workspace, args=["--read-sha256", *flags], env={"HOME": str(tmp_path)})
        binary = payload(client, "read_file", {"path": raw.name})
        assert binary == {"binary": True, "size": raw.stat().st_size, "sha256": digest(raw)}
        first = payload(client, "read_file", {"path": txt.name})
        again = payload(client, "read_file", {"path": txt.name})
        assert first["sha256"] == again["sha256"] == digest(txt) and again["unchanged"]
    limited = daimonos_factory(workspace, args=["--read-sha256", "--tool-input-bytes", "2"])
    failure = limited.call_tool("read_file", {"path": txt.name, "limit": 1})
    assert failure["isError"] and json.loads(failure["content"][0]["text"])["code"] == "tool_input_limit"
    assert "sha256" not in failure["content"][0]["text"]
    plain = daimonos_factory(workspace)
    assert payload(plain, "read_file", {"path": txt.name}) == {"content": "text", "lines": 1}


def test_pinned_git_digest_uses_blob_and_retains_binary_error(daimonos_factory, tmp_path):
    from test_git_read import git, repository
    repo, _ = repository(tmp_path)
    binary = repo / "binary"
    binary.write_bytes(b"DO NOT RENDER\xff\x00")
    git(repo, "add", "binary")
    git(repo, "commit", "-m", "binary")
    rev = git(repo, "rev-parse", "HEAD")
    text_hash, binary_hash = digest(repo / "file.txt"), digest(binary)
    (repo / "file.txt").write_text("uncommitted")
    binary.unlink()
    config = tmp_path / "config.toml"
    config.write_text("[git_read]\nstrict_pinning=true\n")
    client = daimonos_factory(tmp_path, args=["--read-sha256", "--config", str(config)])
    args = {"command": "show_file", "repo": "source", "rev": rev, "path": "file.txt", "limit": 1}
    page = payload(client, "git_read", args)
    assert page["sha256"] == text_hash and page["content"] == "first line\n"
    result = client.call_tool("git_read", {**args, "path": "binary"})
    assert result["isError"]
    error = json.loads(result["content"][0]["text"])
    assert error["error"]["kind"] == "binary_file" and error["metadata"]["sha256"] == binary_hash
    assert "DO NOT RENDER" not in result["content"][0]["text"]
    assert client.call_tool("git_read", {**args, "rev": "HEAD"})["isError"]


def test_pinned_digest_budget_counts_blob_not_response(daimonos_factory, tmp_path):
    from test_git_read import repository
    repo, commit = repository(tmp_path)
    size = (repo / "file.txt").stat().st_size
    args = {"command": "show_file", "repo": "source", "rev": commit, "path": "file.txt", "limit": 1}
    for cap in (size, size - 1):
        client = daimonos_factory(tmp_path, args=["--read-sha256", "--tool-input-bytes", str(cap)])
        result = client.call_tool("git_read", args)
        value = json.loads(result["content"][0]["text"])
        if cap == size:
            assert not result.get("isError") and value["sha256"] == digest(repo / "file.txt"), value
        else:
            assert result["isError"] and value["code"] == "tool_input_limit" and "sha256" not in value


@pytest.mark.parametrize("spill_args", [[], ["--no-output-spill"]], ids=["spill", "no-spill"])
def test_digest_survives_output_caps_on_both_tools(daimonos_factory, tmp_path, spill_args):
    from test_git_read import git, repository
    repo, _ = repository(tmp_path)
    path = repo / "large"
    path.write_bytes(b"line is longer than the output cap. " * 12000)
    git(repo, "add", "large")
    git(repo, "commit", "-m", "large")
    rev = git(repo, "rev-parse", "HEAD")
    config = tmp_path / "caps.toml"
    config.write_text("[git_read]\nmax_file_bytes=128\nmax_patch_bytes=128\nmax_output_bytes=512\n[tool_output]\nmax_bytes=256\n")
    client = daimonos_factory(tmp_path, args=["--read-sha256", *spill_args, "--config", str(config)], env={"HOME": str(tmp_path)})
    for tool, args in [("read_file", {"path": "source/large"}),
                       ("git_read", {"command": "show_file", "repo": "source", "rev": rev, "path": "large"})]:
        result = client.call_tool(tool, args)
        text = result["content"][0]["text"]
        assert len(text.encode()) <= 256
        data = json.loads(text)
        if tool == "git_read":
            assert result["isError"] and data["error"]["kind"] == "output_limit"
            data = data["metadata"]
        assert data["sha256"] == digest(path), (tool, text)


@pytest.mark.parametrize("digest_args", [[], ["--read-sha256"]], ids=["plain", "digest"])
def test_composite_failure_never_exposes_a_partial_digest(daimonos_factory, tmp_path, digest_args):
    from test_git_read import repository
    repo, rev = repository(tmp_path)
    src = repo / "file.txt"
    read = {"tool": "read_file", "arguments": {"path": "source/file.txt", "limit": 1}}
    blob = {"tool": "git_read", "arguments": {"command": "show_file", "repo": "source", "rev": rev, "path": "file.txt"}}
    client = daimonos_factory(tmp_path, args=[*digest_args, "--tool-input-bytes", str(src.stat().st_size)])
    children = payload(client, "batch", {"ops": [read, blob]})
    assert children[0]["ok"]
    assert (digest(src) in json.dumps(children[0])) == bool(digest_args)
    assert not children[1]["ok"] and json.loads(children[1]["error"])["code"] == "tool_input_limit"
    assert "sha256" not in children[1]["error"]
    deadline = daimonos_factory(tmp_path, args=[*digest_args, "--tool-timeout-ms", "300"])
    children = payload(deadline, "batch", {"ops": [{"tool": "exec", "arguments": {"command": "sleep", "args": ["30"]}}, read, blob]})
    for child in children:
        assert not child["ok"] and json.loads(child["error"])["code"] == "tool_timeout"
        assert "sha256" not in child["error"]


def test_digest_cannot_bypass_confinement_or_no_follow(daimonos_factory, tmp_path):
    root, state = tmp_path / "repo", tmp_path / "state"
    root.mkdir()
    state.mkdir(mode=0o700)
    outside = tmp_path / "sensitive"
    outside.write_text("not accessible to the reader")
    (root / "alias").symlink_to(outside)
    client = daimonos_factory(root, args=["--read-sha256", "--confine-paths", "--no-follow-symlinks", "--state-dir", str(state)])
    refused = client.call_tool("read_file", {"path": "alias"})
    assert refused["isError"]
    data = json.loads(refused["content"][0]["text"])
    assert data["code"] == "symlink_refused" and "sha256" not in data and "content" not in data


def test_empty_digest_and_same_length_replacement(daimonos_factory, tmp_path):
    path = tmp_path / "mutable"
    client = daimonos_factory(tmp_path, args=["--read-sha256"])
    for raw in (b"", b"abc", b"xyz"):
        path.write_bytes(raw)
        result = payload(client, "read_file", {"path": path.name})
        assert result["sha256"] == digest(path)
        assert result["content"].encode() == raw


def test_digest_cap_rejects_only_an_impossible_metadata_envelope(daimonos_binary, tmp_path):
    import os
    cfg = tmp_path / "config.toml"
    minimal = {"error": {"kind": "output_limit", "message": "response exceeds byte cap"}, "truncated": True,
               "metadata": {"sha256": "0" * 64}}
    size = len(json.dumps(minimal, separators=(",", ":")))
    for cap in (size - 1, size):
        cfg.write_text(f"[git_read]\nmax_output_bytes={cap}\nmax_file_bytes=1\nmax_patch_bytes=1\n")
        run = subprocess.run([daimonos_binary, "-w", str(tmp_path), "--config", str(cfg), "--read-sha256", "mcp"],
                             env={"HOME": str(tmp_path), "PATH": os.defpath}, stdin=subprocess.DEVNULL,
                             text=True, capture_output=True, timeout=5)
        if cap < size:
            assert run.returncode == 2 and '"reason":"digest_metadata_cap"' in run.stderr, run.stderr
        else:
            assert run.returncode == 0, run.stderr


@pytest.mark.parametrize("digest_args", [[], ["--read-sha256"]], ids=["plain", "digest"])
def test_agent_records_genuine_git_input_hit_without_a_digest(daimonos_binary, tmp_path, digest_args):
    from test_git_read import repository
    from test_tool_budgets import _FakeProvider, _agent_env_file, _isolated_env
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    repo, commit = repository(workspace)
    args = {"command": "show_file", "repo": "source", "rev": commit, "path": "file.txt"}
    size = (repo / "file.txt").stat().st_size
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    audit = state / "audit.jsonl"
    provider = _FakeProvider([("call", "git_read", args), ("end", "done")])
    try:
        envfile = _agent_env_file(tmp_path, provider, allowed="git_read")
        run = subprocess.run([daimonos_binary, "-w", str(workspace), "--state-dir", str(state),
                              "--audit-path", str(audit), *digest_args, "--tool-input-bytes", str(size - 1),
                              "agent", "--print", "--output", "json", "--agent-env", str(envfile), "task"],
                             capture_output=True, text=True, env=_isolated_env(tmp_path), timeout=60)
        assert run.returncode == 0, run.stderr
        reply = "".join(provider.received_tool_results)
        assert "tool_input_limit" in reply and "sha256" not in reply
    finally:
        provider.close()
    hits = [event for line in audit.read_text().splitlines() if (event := json.loads(line))["kind"] == "limit_hit"]
    assert len(hits) == 1 and hits[0]["tool"] == "git_read"
    assert hits[0]["configured"] == size - 1 and hits[0]["consumed"] == size


def test_digest_at_evidence_cutoff_after_branch_advances(daimonos_factory, tmp_path):
    from test_git_read import git, repository
    repo, cutoff = repository(tmp_path)
    expected = digest(repo / "file.txt")
    (repo / "file.txt").write_text("new evidence after cutoff\n")
    git(repo, "commit", "-am", "move branch past cutoff")
    assert git(repo, "rev-parse", "HEAD") != cutoff
    config = tmp_path / "strict.toml"
    config.write_text("[git_read]\nstrict_pinning=true\n")
    client = daimonos_factory(tmp_path, args=["--read-sha256", "--config", str(config)])
    args = {"command": "show_file", "repo": "source", "path": "file.txt", "rev": cutoff, "limit": 1}
    page = payload(client, "git_read", args)
    assert page["commits"] == [cutoff] and page["sha256"] == expected and page["content"] == "first line\n"
    assert client.call_tool("git_read", {**args, "rev": "main"})["isError"]


def test_unconfined_digest_refuses_fifo_without_retry(daimonos_factory, tmp_path):
    import os
    os.mkfifo(tmp_path / "pipe")
    client = daimonos_factory(tmp_path, args=["--read-sha256"])
    error = client.call_tool("read_file", {"path": "pipe"})
    assert error["isError"]
    refusal = json.loads(error["content"][0]["text"])
    assert refusal["code"] == "unsupported_file_type" and refusal["retryable"] is False
    assert "sha256" not in refusal


def test_git_replace_refs_do_not_substitute_blob_evidence(daimonos_factory, tmp_path):
    from test_git_read import git, repository
    repo, commit = repository(tmp_path)
    path = repo / "file.txt"
    expected = digest(path)
    original = git(repo, "rev-parse", "HEAD:file.txt")
    path.write_bytes(b"replacement bytes\n")
    alternate = git(repo, "hash-object", "-w", "file.txt")
    git(repo, "replace", original, alternate)
    assert git(repo, "cat-file", "blob", original) == "replacement bytes"
    client = daimonos_factory(tmp_path, args=["--read-sha256"])
    page = payload(client, "git_read", {"command": "show_file", "repo": "source", "rev": commit, "path": "file.txt"})
    assert page["sha256"] == expected and page["content"] == "first line\nsecond line\n"
