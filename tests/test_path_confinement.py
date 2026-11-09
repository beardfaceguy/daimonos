"""End-to-end confined read behavior (ADR-022)."""

import json

import pytest


@pytest.fixture
def confined(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside.txt"
    outside.write_text("OUTSIDE_SENTINEL")
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths"])
    return client, workspace, outside


def test_confined_read_rejects_outside_file(confined):
    client, workspace, outside = confined
    (workspace / "inside.txt").write_text("inside")
    accepted = client.call_tool("read_file", {"path": "inside.txt"})
    assert json.loads(accepted["content"][0]["text"])["content"] == "inside"

    refused = client.call_tool("read_file", {"path": str(outside)})
    assert refused.get("isError") is True
    assert "OUTSIDE_SENTINEL" not in json.dumps(refused)


def test_confined_root_does_not_load_workspace_env_outside_root(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    root = workspace / "root"
    root.mkdir()
    (root / "inside.txt").write_text("inside")
    (workspace / "agent.env").write_text("DAIMONOS_KGL_AUTOINDEX=1\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths",
                                               "--confine-root", str(root)])
    accepted = client.call_tool("read_file", {"path": "inside.txt"})
    assert json.loads(accepted["content"][0]["text"])["content"] == "inside"


@pytest.mark.parametrize("setting", ["DAIMONOS_KGL_AUTOINDEX", "DAIMONOS_KGL_OBSERVE"])
def test_confined_loads_in_root_workspace_env(daimonos_binary, tmp_path, setting):
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "agent.env").write_text(f"{setting}=1\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "--mcp"],
        input="", capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "KGL" in error["message"]


def test_confined_does_not_follow_workspace_env_symlink_outside(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("inside")
    outside = tmp_path / "outside.env"
    outside.write_text("DAIMONOS_KGL_AUTOINDEX=1\n")
    (workspace / "agent.env").symlink_to(outside)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths"])
    accepted = client.call_tool("read_file", {"path": "inside.txt"})
    assert json.loads(accepted["content"][0]["text"])["content"] == "inside"


@pytest.mark.parametrize("explicit_config", [False, True])
def test_confined_honors_authorized_and_explicit_config(daimonos_factory, tmp_path, explicit_config):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("inside")
    (workspace / "link.txt").symlink_to("inside.txt")
    config = tmp_path / "explicit.toml" if explicit_config else workspace / "daimonos.toml"
    config.write_text("[reviewer.tool_limits]\nmax_symlinks = 0\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    args = ["--state-dir", str(state), "--confine-paths"]
    if explicit_config:
        args += ["--config", str(config)]
    client = daimonos_factory(workspace, args=args)
    refused = client.call_tool("read_file", {"path": "link.txt"})
    assert refused.get("isError") is True
    error = json.loads(refused["content"][0]["text"])
    assert error["code"] == "limit_exceeded"
    assert error["configured"] == 0
    assert error["consumed"] == 1
    assert "inside" not in json.dumps(refused)


def test_confined_read_does_not_erase_symlink_before_parent_component(confined, tmp_path):
    client, workspace, _ = confined
    (workspace / "inside.txt").write_text("INSIDE_SENTINEL")
    outside_dir = tmp_path / "outside-dir"
    outside_dir.mkdir()
    (workspace / "escape").symlink_to(outside_dir, target_is_directory=True)
    refused = client.call_tool("read_file", {"path": "escape/../inside.txt"})
    assert refused.get("isError") is True
    assert "INSIDE_SENTINEL" not in json.dumps(refused)


def test_confined_read_resolves_parent_after_in_root_symlink(confined):
    client, workspace, _ = confined
    nested = workspace / "nested" / "child"
    nested.mkdir(parents=True)
    (workspace / "inside.txt").write_text("inside")
    (workspace / "link").symlink_to(nested, target_is_directory=True)
    accepted = client.call_tool("read_file", {"path": "link/../../inside.txt"})
    assert not accepted.get("isError"), accepted
    assert json.loads(accepted["content"][0]["text"])["content"] == "inside"


def test_confined_cwd_stays_inside_root_and_controls_relative_reads(confined, tmp_path):
    client, workspace, _ = confined
    (workspace / "item.txt").write_text("ROOT_SENTINEL")
    sub = workspace / "sub"
    sub.mkdir()
    (sub / "item.txt").write_text("sub")
    changed = client.call_tool("set_cwd", {"path": "sub"})
    assert not changed.get("isError"), changed
    read = client.call_tool("read_file", {"path": "item.txt"})
    assert json.loads(read["content"][0]["text"])["content"] == "sub"

    refused = client.call_tool("set_cwd", {"path": str(tmp_path)})
    assert refused.get("isError") is True
    assert "ROOT_SENTINEL" not in json.dumps(refused)
    after = client.call_tool("read_file", {"path": "item.txt"})
    assert json.loads(after["content"][0]["text"]) == {"unchanged": True, "lines": 1}


def test_client_root_outside_confined_root_does_not_reroot(daimonos_binary, tmp_path):
    import subprocess
    import time
    from test_roots import RootsAwareClient

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("inside")
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "inside.txt").write_text("OUTSIDE_SENTINEL")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    process = subprocess.Popen(
        [daimonos_binary, "--mcp", "--workspace", str(workspace),
         "--state-dir", str(state), "--confine-paths"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    client = RootsAwareClient(process, [{"uri": f"file://{outside}", "name": "outside"}])
    try:
        initialized = client.request("initialize", {
            "protocolVersion": "2025-11-25", "capabilities": {"roots": {"listChanged": True}},
            "clientInfo": {"name": "pytest", "version": "1"},
        })
        assert "result" in initialized, initialized
        client.notify("notifications/initialized")
        time.sleep(0.3)
        response = client.call_tool("read_file", {"path": "inside.txt"})
        assert not response.get("isError"), response
        assert json.loads(response["content"][0]["text"])["content"] == "inside"
        assert "OUTSIDE_SENTINEL" not in json.dumps(response)
    finally:
        process.terminate()
        process.wait(timeout=5)


def test_confined_cwd_returns_real_in_root_directory(confined):
    client, workspace, _ = confined
    sub = workspace / "sub"
    sub.mkdir()
    (sub / "item.txt").write_text("inside")
    (workspace / "alias").symlink_to("sub", target_is_directory=True)
    changed = client.call_tool("set_cwd", {"path": "alias"})
    assert not changed.get("isError"), changed
    assert json.loads(changed["content"][0]["text"])["cwd"] == str(sub)
    read = client.call_tool("read_file", {"path": "item.txt"})
    assert json.loads(read["content"][0]["text"])["content"] == "inside"


def test_confined_skill_cannot_read_linked_outside_file(confined):
    client, workspace, outside = confined
    external_root = outside.parent / "external-skills"
    skill = external_root / "skills" / "leak"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: leak\ndescription: outside\n---\nOUTSIDE_SENTINEL\n")
    (workspace / ".agents").symlink_to(external_root, target_is_directory=True)
    refused = client.call_tool("skill", {"name": "leak"})
    assert refused.get("isError") is True
    assert "OUTSIDE_SENTINEL" not in json.dumps(refused)


def test_confined_tool_list_hides_unimplemented_or_unsafe_tools(confined):
    client, _, _ = confined
    names = {tool["name"] for tool in client.list_tools()}
    assert "read_file" in names
    assert "exec" not in names
    assert "write_file" not in names
    assert "skill" not in names
    assert "git" not in names


def test_confined_ls_follows_in_root_links_and_skips_outside_links(confined, tmp_path):
    client, workspace, _ = confined
    safe = workspace / "safe"
    safe.mkdir()
    (safe / "visible.txt").write_text("visible")
    (workspace / "alias").symlink_to(safe, target_is_directory=True)
    external = tmp_path / "outside-dir"
    external.mkdir()
    (external / "OUTSIDE_SECRET_NAME").write_text("OUTSIDE_SENTINEL")
    (workspace / "escape").symlink_to(external, target_is_directory=True)

    result = client.call_tool("ls", {"path": ".", "depth": 2})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    names = {entry["n"] for entry in payload["entries"]}
    assert "alias/visible.txt" in names
    assert "OUTSIDE_SECRET_NAME" not in json.dumps(payload)
    assert "OUTSIDE_SENTINEL" not in json.dumps(payload)
    assert payload.get("skipped", 0) >= 1
    assert payload.get("skipped_links", 0) >= 1


def test_confined_ls_reports_walk_budget_exhaustion(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    for name in ["a", "b", "c"]:
        (workspace / name).write_text(name)
    config = tmp_path / "config.toml"
    config.write_text("[index]\nmax_files = 1\nmax_walk_entries = 2\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths",
                                               "--config", str(config)])
    result = client.call_tool("ls", {"path": "."})
    assert result.get("isError") is True
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "limit_exceeded"
    assert error["tool"] == "ls"
    assert error["configured"] == 2
    assert error["consumed"] == 2
    assert error["retryable"] is False


def test_confined_ls_allows_exact_walk_budget(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    for name in ["a", "b"]:
        (workspace / name).write_text(name)
    config = tmp_path / "config.toml"
    config.write_text("[index]\nmax_files = 1\nmax_walk_entries = 2\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths",
                                               "--config", str(config)])
    result = client.call_tool("ls", {"path": "."})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert {entry["n"] for entry in payload["entries"]} == {"a", "b"}


def test_confined_ls_reports_in_root_symlink_cycle(confined):
    client, workspace, _ = confined
    directory = workspace / "dir"
    directory.mkdir()
    (directory / "back").symlink_to(workspace, target_is_directory=True)
    result = client.call_tool("ls", {"path": ".", "depth": 5})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert payload.get("skipped", 0) >= 1
    assert len(payload["entries"]) <= 3


def test_confined_content_search_skips_outside_links(confined):
    client, workspace, outside = confined
    (workspace / "public.txt").write_text("needle public\n")
    outside.write_text("needle OUTSIDE_SENTINEL\n")
    (workspace / "escape.txt").symlink_to(outside)
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert any(match["f"].endswith("public.txt") for match in payload["matches"])
    assert "OUTSIDE_SENTINEL" not in json.dumps(payload)
    assert payload.get("skipped", 0) >= 1


def test_confined_content_search_honors_workspace_gitignore(confined):
    client, workspace, _ = confined
    (workspace / ".git").mkdir()
    (workspace / ".gitignore").write_text("secret.txt\n")
    (workspace / "secret.txt").write_text("needle IGNORED_SENTINEL\n")
    (workspace / "public.txt").write_text("needle visible\n")
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert any(match["f"].endswith("public.txt") for match in payload["matches"])
    assert "IGNORED_SENTINEL" not in json.dumps(payload)
    assert "secret.txt" not in json.dumps(payload)


def test_confined_content_search_honors_git_info_exclude(confined):
    client, workspace, _ = confined
    (workspace / ".git" / "info").mkdir(parents=True)
    (workspace / ".git" / "info" / "exclude").write_text("secret.txt\n")
    (workspace / "secret.txt").write_text("needle EXCLUDED_SENTINEL\n")
    (workspace / "public.txt").write_text("needle visible\n")
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert any(match["f"].endswith("public.txt") for match in payload["matches"])
    assert "EXCLUDED_SENTINEL" not in json.dumps(payload)
    assert "secret.txt" not in json.dumps(payload)


def test_confined_search_honors_nested_ignore_and_negation(confined):
    client, workspace, _ = confined
    (workspace / ".git").mkdir()
    (workspace / ".gitignore").write_text("*.txt\n")
    sub = workspace / "sub"
    sub.mkdir()
    (sub / ".gitignore").write_text("!allowed.txt\n")
    (sub / "allowed.txt").write_text("needle allowed\n")
    (sub / "blocked.txt").write_text("needle BLOCKED_SENTINEL\n")
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert any(match["f"].endswith("allowed.txt") for match in payload["matches"])
    assert "BLOCKED_SENTINEL" not in json.dumps(payload)


def test_confined_content_search_accepts_explicit_file(confined):
    client, workspace, _ = confined
    (workspace / "public.txt").write_text("needle visible\n")
    result = client.call_tool("search", {"pattern": "needle", "mode": "content", "path": "public.txt"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert len(payload["matches"]) == 1
    assert payload["matches"][0]["f"] == "public.txt"


def test_confined_filename_search_skips_outside_and_ignored_files(confined, tmp_path):
    client, workspace, _ = confined
    (workspace / ".git").mkdir()
    (workspace / ".gitignore").write_text("ignored-public.txt\n")
    (workspace / "public.txt").write_text("visible")
    (workspace / "ignored-public.txt").write_text("ignored")
    external = tmp_path / "external"
    external.mkdir()
    (external / "outside-public.txt").write_text("outside")
    (workspace / "escape").symlink_to(external, target_is_directory=True)
    result = client.call_tool("search", {"pattern": "public", "mode": "files"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    names = [entry["file"] for entry in payload["results"]]
    assert "public.txt" in names
    assert not any("ignored-public" in name or "outside-public" in name for name in names)
    assert payload.get("skipped", 0) >= 1


def test_confined_filename_search_accepts_explicit_file(confined):
    client, workspace, _ = confined
    (workspace / "public.txt").write_text("visible")
    (workspace / "public-other.txt").write_text("other")
    result = client.call_tool("search", {"pattern": "public", "mode": "files", "path": "public.txt"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert [entry["file"] for entry in payload["results"]] == ["public.txt"]


def test_confined_diff_rejects_outside_second_file(confined):
    client, workspace, outside = confined
    (workspace / "first.txt").write_text("inside\n")
    (workspace / "second.txt").write_text("changed\n")
    inside = client.call_tool("diff_files", {"path_a": "first.txt", "path_b": "second.txt"})
    assert not inside.get("isError"), inside
    assert not json.loads(inside["content"][0]["text"])["identical"]
    refused = client.call_tool("diff_files", {"path_a": "first.txt", "path_b": str(outside)})
    assert refused.get("isError") is True
    assert "OUTSIDE_SENTINEL" not in json.dumps(refused)


def test_confined_script_cannot_bypass_read_or_execute_policy(confined, tmp_path):
    client, _, outside = confined
    read = client.call_tool("execute_script", {"code": f"result = read_file({str(outside)!r})"})
    assert "OUTSIDE_SENTINEL" not in json.dumps(read)
    assert "path_outside_root" in json.dumps(read)

    marker = tmp_path / "command-ran"
    code = f'result = exec("touch", [{str(marker)!r}])'
    execution = client.call_tool("execute_script", {"code": code})
    assert "filesystem_policy_unsupported" in json.dumps(execution)
    assert not marker.exists()


def test_confined_batch_enforces_each_nested_policy(confined, tmp_path):
    client, _, outside = confined
    marker = tmp_path / "command-ran"
    response = client.call_tool("batch", {"ops": [
        {"tool": "read_file", "arguments": {"path": str(outside)}},
        {"tool": "exec", "arguments": {"command": "touch", "args": [str(marker)]}},
    ]})
    text = json.dumps(response)
    assert "OUTSIDE_SENTINEL" not in text
    assert "path_outside_root" in text
    assert "filesystem_policy_unsupported" in text
    assert not marker.exists()


@pytest.mark.parametrize("outside_root", [False, True])
def test_confined_acp_advertises_only_in_root_skills(daimonos_binary, tmp_path, outside_root):
    import os
    import subprocess
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    external = tmp_path / "external-skills" if outside_root else workspace / "in-root-skills"
    skill = external / "skills" / "leak"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: leak\ndescription: OUTSIDE_SENTINEL\n---\nsecret\n")
    (workspace / ".agents").symlink_to(external, target_is_directory=True)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        "",
    ]))
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
               XDG_DATA_HOME=str(home / "data"), TMPDIR=str(tmp_path))
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "acp", "--agent-env", str(agent_env)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        initialized = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "result" in initialized, initialized
        created = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
            "cwd": str(workspace), "mcpServers": [],
        }})
        assert "sessionId" in created.get("result", {}), created
        for request_id in range(3, 13):
            notifications = json.dumps(client.notifications)
            if '"clear"' in notifications and '"usage"' in notifications:
                break
            client.send_raw({"jsonrpc": "2.0", "id": request_id,
                             "method": "session/list", "params": {}})
        notifications = json.dumps(client.notifications)
        assert '"clear"' in notifications and '"usage"' in notifications
        assert ("OUTSIDE_SENTINEL" in notifications) is (not outside_root)
    finally:
        if process.poll() is None:
            _terminate(process)


def test_confined_skill_loads_authorized_file(confined):
    client, workspace, _ = confined
    skill = workspace / ".agents" / "skills" / "inside"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: inside\ndescription: in root\n---\nVISIBLE_BODY\n")
    result = client.call_tool("skill", {"name": "inside"})
    assert not result.get("isError"), result
    assert "VISIBLE_BODY" in json.dumps(result)


def test_confined_server_instructions_do_not_inspect_outside_workspace(daimonos_binary, tmp_path):
    import subprocess
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    root = workspace / "root"
    root.mkdir()
    (root / "inside-dir").mkdir()
    (workspace / "OUTSIDE_DIR_SENTINEL").mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    process = subprocess.Popen(
        [daimonos_binary, "--mcp", "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "--confine-root", str(root)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        response = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "OUTSIDE_DIR_SENTINEL" not in json.dumps(response)
        assert "inside-dir" in json.dumps(response)
    finally:
        if process.poll() is None:
            _terminate(process)


def test_confined_search_does_not_skip_unignored_directories(confined):
    client, workspace, _ = confined
    directory = workspace / "node_modules" / "package"
    directory.mkdir(parents=True)
    (directory / "visible.txt").write_text("needle visible\n")
    content = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not content.get("isError"), content
    content_data = json.loads(content["content"][0]["text"])
    assert any(match["f"].endswith("visible.txt") for match in content_data["matches"])
    names = client.call_tool("search", {"pattern": "visible", "mode": "files"})
    assert not names.get("isError"), names
    names_data = json.loads(names["content"][0]["text"])
    assert any(item["file"].endswith("visible.txt") for item in names_data["results"])


def test_confined_unclassified_tool_returns_structured_failure(confined, tmp_path):
    client, _, _ = confined
    marker = tmp_path / "command-ran"
    result = client.call_tool("exec", {"command": "touch", "args": [str(marker)]})
    assert result.get("isError") is True
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "filesystem_policy_unsupported"
    assert error["tool"] == "exec"
    assert error["retryable"] is False
    assert not marker.exists()


def test_confined_failure_reports_original_relative_argument(confined):
    client, _, _ = confined
    result = client.call_tool("read_file", {"path": "../outside.txt"})
    assert result.get("isError") is True
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "path_outside_root"
    assert error["tool"] == "read_file"
    assert error["argument"] == "../outside.txt"


@pytest.mark.parametrize("tool,args,bad_argument", [
    ("ls", {"path": "../outside"}, "../outside"),
    ("search", {"pattern": "x", "mode": "content", "path": "../outside"}, "../outside"),
    ("search", {"pattern": "x", "mode": "files", "path": "../outside"}, "../outside"),
    ("diff_files", {"path_a": "inside.txt", "path_b": "../outside.txt"}, "../outside.txt"),
    ("set_cwd", {"path": "../outside"}, "../outside"),
])
def test_confined_failures_preserve_caller_path(confined, tool, args, bad_argument):
    client, workspace, _ = confined
    (workspace / "inside.txt").write_text("inside")
    result = client.call_tool(tool, args)
    assert result.get("isError") is True
    error = json.loads(result["content"][0]["text"])
    assert error["argument"] == bad_argument


def test_confined_prompt_dump_requires_explicit_output(daimonos_binary, tmp_path):
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--confine-paths", "--dump-prompts"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2, completed.stderr
    assert "--dump-prompts" in completed.stderr
    assert list(home.iterdir()) == []
    assert list(workspace.iterdir()) == []


@pytest.mark.parametrize("explicit_config", [False, True])
def test_confined_implicit_config_cannot_load_outside_prompt(daimonos_binary, tmp_path, explicit_config):
    import subprocess
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside-prompt.md"
    outside.write_text("OUTSIDE_PROMPT_SENTINEL")
    config = workspace / "daimonos.toml"
    config.write_text("[prompts]\nmcp_instructions = " + json.dumps(str(outside)) + "\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    command = [daimonos_binary, "--mcp", "--workspace", str(workspace),
               "--state-dir", str(state), "--confine-paths"]
    if explicit_config:
        command += ["--config", str(config)]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        client = DaimonosClient(process, str(workspace))
        response = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "result" in response, response
        assert ("OUTSIDE_PROMPT_SENTINEL" in json.dumps(response)) is explicit_config
    finally:
        if process.poll() is None:
            _terminate(process)


def test_confined_acp_refuses_forwarded_stdio_server(daimonos_binary, tmp_path):
    import os
    import subprocess
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    marker = tmp_path / "stdio-started"
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    process = subprocess.Popen(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "acp", "--agent-env", str(agent_env)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        initialized = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1, "clientCapabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "result" in initialized, initialized
        created = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
            "cwd": str(workspace), "mcpServers": [{
                "name": "probe", "command": "/usr/bin/touch", "args": [str(marker)], "env": [],
            }],
        }})
        assert "error" in created, created
        assert "filesystem_policy_unsupported" in json.dumps(created)
        assert "probe" in json.dumps(created)
        assert not marker.exists()
    finally:
        if process.poll() is None:
            _terminate(process)


@pytest.mark.parametrize("explicit_config", [False, True])
def test_confined_acp_limits_configured_zed_settings(daimonos_binary, tmp_path, explicit_config):
    import os
    import subprocess
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    marker = tmp_path / "stdio-started"
    settings = tmp_path / "outside-settings.json"
    settings.write_text(json.dumps({"context_servers": {
        "probe": {"command": "/usr/bin/touch", "args": [str(marker)]},
    }}))
    config = workspace / "daimonos.toml"
    config.write_text("[acp.mcp]\nzed_settings_path = " + json.dumps(str(settings)) + "\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1", "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test", "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off", "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state), "--confine-paths"]
    if explicit_config:
        command += ["--config", str(config)]
    process = subprocess.Popen(command + ["acp", "--agent-env", str(agent_env)],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    try:
        client = DaimonosClient(process, str(workspace))
        initialized = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "Zed", "version": "1"},
        }})
        assert "result" in initialized, initialized
        created = client.send_raw({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
            "cwd": str(workspace), "mcpServers": [],
        }})
        if explicit_config:
            assert "error" in created and "probe" in json.dumps(created), created
        else:
            assert "sessionId" in created.get("result", {}), created
        assert not marker.exists()
    finally:
        if process.poll() is None:
            _terminate(process)


@pytest.mark.parametrize("explicit_config", [False, True])
def test_confined_agent_limits_configured_mcp_settings(daimonos_binary, tmp_path, explicit_config):
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    marker = tmp_path / "stdio-started"
    settings = tmp_path / "zed" / "settings.json"
    settings.parent.mkdir()
    settings.write_text(json.dumps({"context_servers": {
        "probe": {"command": "/usr/bin/touch", "args": [str(marker)]},
    }}))
    config = workspace / "daimonos.toml"
    config.write_text("[agent.mcp]\nservers_file = " + json.dumps(str(settings)) + "\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    command = [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state), "--confine-paths"]
    if explicit_config:
        command += ["--config", str(config)]
    completed = subprocess.run(command + ["agent", "--dry-run", "inspect"],
                               capture_output=True, text=True, timeout=10, env=env)
    if explicit_config:
        assert completed.returncode == 2, completed.stderr
        assert "filesystem_policy_unsupported" in completed.stderr
        assert "probe" in completed.stderr
        assert not (state / "logs").exists()
    else:
        assert completed.returncode == 0, completed.stderr
    assert not marker.exists()


@pytest.mark.parametrize("env_source", ["none", "parent", "workspace"])
def test_confined_agent_requires_explicit_agent_env(daimonos_binary, tmp_path, env_source):
    import os
    import subprocess
    from acp_crash_probe import BASELINE, MockProvider

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    config = home / ".config" / "daimonos"
    config.mkdir(parents=True)
    provider = MockProvider()
    try:
        agent_env = config / "agent.env"
        agent_env.write_text("\n".join([
            "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
            f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.server.server_port}/v1",
            "DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_API_KEY=test",
            "DAIMONOS_AGENT_COMPACTION=off", "DAIMONOS_AGENT_MODELS_DISCOVER=off", "",
        ]))
        env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"), TMPDIR=str(tmp_path))
        if env_source == "parent":
            env["DAIMONOS_AGENT_ENV"] = str(agent_env)
        elif env_source == "workspace":
            (workspace / "agent.env").write_text("DAIMONOS_AGENT_ENV=" + str(agent_env) + "\n")
        completed = subprocess.run(
            [daimonos_binary, "--workspace", str(workspace), "--confine-paths", "agent", "--print", "hello"],
            capture_output=True, text=True, timeout=30, env=env,
        )
        if env_source == "parent":
            assert completed.returncode == 0, completed.stderr
            assert BASELINE in completed.stdout
        else:
            assert completed.returncode != 0
            assert "--agent-env" in completed.stderr or "DAIMONOS_AGENT_ENV" in completed.stderr
    finally:
        provider.close()


def test_confined_rejects_unscoped_request_trace_from_workspace_env(daimonos_binary, tmp_path):
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside-traces"
    (workspace / "agent.env").write_text("DAIMONOS_OPENROUTER_TRACE_DIR=" + str(outside) + "\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--confine-paths", "agent", "--dry-run", "inspect"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "DAIMONOS_OPENROUTER_TRACE_DIR" in error["message"]
    assert not outside.exists()


@pytest.mark.parametrize("inside_root", [False, True])
def test_confined_acp_refuses_explicit_outside_skill_directory(daimonos_binary, tmp_path, inside_root):
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = workspace / "skills" if inside_root else tmp_path / "outside-skills"
    outside.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1", "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_API_KEY=test", "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_SKILL_DIR=" + str(outside), "",
    ]))
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "acp", "--agent-env", str(agent_env)],
        input="", capture_output=True, text=True, timeout=10, env=env,
    )
    if inside_root:
        assert completed.returncode == 0, completed.stderr
    else:
        assert completed.returncode != 0
        assert "path_outside_root" in completed.stderr
        assert "DAIMONOS_SKILL_DIR" in completed.stderr


def test_confined_symlink_target_budget_is_configurable(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    target = "a" * 80
    (workspace / target).write_text("inside")
    (workspace / "link").symlink_to(target)
    config = tmp_path / "config.toml"
    config.write_text("[reviewer.tool_limits]\nmax_symlink_target_bytes = 32\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths",
                                               "--config", str(config)])
    result = client.call_tool("read_file", {"path": "link"})
    assert result.get("isError") is True
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "limit_exceeded"
    assert error["configured"] == 32
    assert error["consumed"] >= 32


def test_confined_mcp_config_refuses_unimplemented_local_access(daimonos_binary, tmp_path):
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--confine-paths", "mcp-config", "sync", "--dry-run"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "mcp-config" in error["message"]
    assert list(home.iterdir()) == []


def test_confined_discovery_does_not_advertise_unconfined_tools(confined):
    client, _, _ = confined
    listed = client.call_tool("list_all_tools", {})
    assert not listed.get("isError"), listed
    definitions = json.loads(listed["content"][0]["text"])
    names = {definition["name"] for definition in definitions}
    assert "read_file" in names
    assert "write_file" not in names
    assert "exec" not in names
    signatures = client.call_tool("list_tool_signatures", {})
    assert "def exec(" not in json.dumps(signatures)
    assert "def cargo(" not in json.dumps(signatures)
    schema = client.call_tool("get_tool_schema", {"tools": ["exec"]})
    assert schema.get("isError") is True
    assert "filesystem_policy_unsupported" in json.dumps(schema)


def test_confined_ls_reports_depth_budget_exhaustion(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    (workspace / "one" / "two").mkdir(parents=True)
    (workspace / "one" / "two" / "inside.txt").write_text("inside")
    config = tmp_path / "config.toml"
    config.write_text("[index]\nmax_depth = 1\n")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-paths",
                                               "--config", str(config)])
    result = client.call_tool("ls", {"path": ".", "depth": 3})
    assert result.get("isError") is True
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "limit_exceeded"
    assert error["configured"] == 1
    assert error["consumed"] == 2
    assert "max_walk_depth" in error["message"]


def test_confined_native_socket_stat_and_glob(daimonos_binary, tmp_path):
    import socket
    import subprocess
    import tempfile
    import time
    from pathlib import Path

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("inside")
    outside = tmp_path / "outside.txt"
    outside.write_text("OUTSIDE_SENTINEL")
    (workspace / "escape.txt").symlink_to(outside)
    with tempfile.TemporaryDirectory(prefix="dn-") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        socket_path = state / "native.sock"
        process = subprocess.Popen(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "--confine-paths", "--socket", str(socket_path)],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 10
            while not socket_path.exists() and time.monotonic() < deadline:
                assert process.poll() is None, process.stderr.read().decode(errors="replace")
                time.sleep(0.01)
            assert socket_path.exists()
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(socket_path))
                with client.makefile("rwb") as stream:
                    for request in [{"c": 4, "p": "escape.txt"}, {"c": 5, "p": "*.txt"}]:
                        stream.write((json.dumps(request) + "\n").encode())
                        stream.flush()
                        response = json.loads(stream.readline())
                        if request["c"] == 4:
                            assert response["ok"] is False
                            assert response["d"]["code"] == "symlink_refused"
                        else:
                            assert response["ok"] is True
                            assert response["d"]["files"] == ["inside.txt"]
                        assert "OUTSIDE_SENTINEL" not in json.dumps(response)
        finally:
            if process.poll() is None:
                process.terminate()
            process.communicate(timeout=5)


def test_confined_content_search_accepts_in_root_file_link(confined):
    client, workspace, _ = confined
    (workspace / "real.txt").write_text("needle visible\n")
    (workspace / "alias.txt").symlink_to("real.txt")
    result = client.call_tool("search", {"pattern": "needle", "mode": "content", "path": "alias.txt"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert [match["f"] for match in payload["matches"]] == ["alias.txt"]


def test_confined_oauth_status_requires_selected_state(daimonos_binary, tmp_path):
    import hashlib
    import os
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    home = tmp_path / "home"
    legacy = home / ".blue_rose" / "mcp-oauth"
    legacy.mkdir(parents=True, mode=0o700)
    (home / ".blue_rose").chmod(0o700)
    endpoint = "https://example.com/mcp"
    key = hashlib.sha256((endpoint + "\0default").encode()).hexdigest()
    credential = legacy / (key + ".json")
    credential.write_text(json.dumps({"client_id": "test", "access_token": "test-token"}))
    credential.chmod(0o600)
    config = tmp_path / "config.toml"
    config.write_text('[acp.mcp.oauth_servers.demo]\nurl = "https://example.com/mcp"\n')
    env = {key: value for key, value in os.environ.items() if not key.startswith("DAIMONOS_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"), TMPDIR=str(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--config", str(config),
         "--confine-paths", "mcp", "auth", "status", "demo"],
        capture_output=True, text=True, timeout=10, env=env,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "--state-dir" in error["message"]
    assert "authenticated" not in completed.stdout


@pytest.mark.skipif(__import__("platform").system() != "Linux" or
                    __import__("platform").machine() not in {"x86_64", "aarch64"},
                    reason="seccomp test supports Linux x86_64 and aarch64")
def test_confined_startup_fails_before_writers_when_openat2_is_unavailable(daimonos_binary, tmp_path):
    import ctypes
    import os
    import subprocess

    class Filter(ctypes.Structure):
        _fields_ = [("code", ctypes.c_ushort), ("jt", ctypes.c_ubyte),
                    ("jf", ctypes.c_ubyte), ("k", ctypes.c_uint)]

    class Program(ctypes.Structure):
        _fields_ = [("length", ctypes.c_ushort), ("filter", ctypes.POINTER(Filter))]

    # Return ENOSYS only for openat2; allow every other syscall.
    load_syscall, jump_equal, return_constant = 0x20, 0x15, 0x06
    sys_openat2, enosys = 437, 38  # Linux x86_64 and aarch64.
    seccomp_errno, seccomp_allow = 0x00050000, 0x7FFF0000
    rules = (Filter * 4)(
        Filter(load_syscall, 0, 0, 0),
        Filter(jump_equal, 0, 1, sys_openat2),
        Filter(return_constant, 0, 0, seccomp_errno | enosys),
        Filter(return_constant, 0, 0, seccomp_allow),
    )
    program = Program(len(rules), rules)
    libc = ctypes.CDLL(None, use_errno=True)

    def disable_openat2():
        set_no_new_privileges, set_seccomp, filter_mode = 38, 22, 2
        if (libc.prctl(set_no_new_privileges, 1, 0, 0, 0) != 0 or
                libc.prctl(set_seccomp, filter_mode, ctypes.byref(program)) != 0):
            os._exit(97)

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "--mcp"],
        input="", capture_output=True, text=True, timeout=10, preexec_fn=disable_openat2,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "openat2" in error["message"]
    assert list(state.iterdir()) == []
    assert list(workspace.iterdir()) == []


def test_confined_startup_refuses_explicit_local_tool_before_writers(daimonos_binary, tmp_path):
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    config = tmp_path / "config.toml"
    config.write_text('[tools.probe]\nbin = "/usr/bin/touch"\n')
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "--config", str(config), "--mcp"],
        input="", capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "probe" in error["message"]
    assert list(state.iterdir()) == []


def test_confined_startup_rejects_invalid_explicit_config(daimonos_binary, tmp_path):
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    config = tmp_path / "config.toml"
    config.write_text('[tools.probe]\ncommand = "/usr/bin/touch"\n')
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "--config", str(config), "--mcp"],
        input="", capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode == 2, completed.stderr
    assert "config" in completed.stderr.lower()
    assert list(state.iterdir()) == []


def test_confined_search_preserves_ignore_file_precedence(confined):
    client, workspace, _ = confined
    (workspace / ".git").mkdir()
    (workspace / ".ignore").write_text("secret.txt\n")
    sub = workspace / "sub"
    sub.mkdir()
    (sub / ".gitignore").write_text("!secret.txt\n")
    (sub / "secret.txt").write_text("needle PRECEDENCE_SENTINEL\n")
    (sub / "public.txt").write_text("needle visible\n")
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert any(match["f"].endswith("public.txt") for match in payload["matches"])
    assert "PRECEDENCE_SENTINEL" not in json.dumps(payload)


def test_confined_search_respects_nested_git_root(confined, daimonos_factory, tmp_path):
    client, workspace, _ = confined
    (workspace / ".git").mkdir()
    (workspace / ".gitignore").write_text("*.txt\n")
    nested = workspace / "nested"
    (nested / ".git").mkdir(parents=True)
    (nested / "visible.txt").write_text("needle visible\n")
    legacy_state = tmp_path / "legacy-state"
    legacy_state.mkdir(mode=0o700)
    legacy = daimonos_factory(workspace, args=["--state-dir", str(legacy_state)])
    control = legacy.call_tool("search", {"pattern": "needle", "mode": "content"})
    control_data = json.loads(control["content"][0]["text"])
    assert any(match["f"] == "nested/visible.txt" for match in control_data["matches"])
    result = client.call_tool("search", {"pattern": "needle", "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert any(match["f"] == "nested/visible.txt" for match in payload["matches"])


def test_confined_prompt_dump_refuses_outside_output(daimonos_binary, tmp_path):
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside"
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--confine-paths",
         "--dump-prompts", str(outside)],
        capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "--dump-prompts" in error["message"]
    assert not outside.exists()


def test_confined_session_import_rejects_outside_archive(daimonos_binary, tmp_path):
    import subprocess
    from test_session_import_export import archive

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside-session.json"
    outside.write_text(json.dumps(archive()))
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "session", "import", str(outside)],
        capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "path_outside_root"
    assert error["argument"] == str(outside)
    assert list(state.iterdir()) == []


def test_confined_session_export_refuses_unmediated_output(daimonos_binary, tmp_path):
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    outside = tmp_path / "outside.json"
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
         "--confine-paths", "session", "export", "unused", "--output", str(outside)],
        capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert not outside.exists()
    assert list(state.iterdir()) == []


def test_confined_agent_debug_output_requires_selected_state(daimonos_binary, tmp_path):
    import subprocess

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside.log"
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--confine-paths",
         "agent", "--debug-thoughts", "--debug-thoughts-path", str(outside),
         "--agent-env", str(tmp_path / "unused.env"), "inspect"],
        capture_output=True, text=True, timeout=10,
    )
    assert completed.returncode == 2, completed.stderr
    error = json.loads(completed.stderr.splitlines()[-1])["error"]
    assert error["code"] == "filesystem_policy_unsupported"
    assert "--state-dir" in error["message"]
    assert not outside.exists()


def test_confined_implicit_config_loads_in_root_prompt(daimonos_binary, tmp_path):
    import subprocess
    from conftest import DaimonosClient, _terminate

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    prompt = workspace / "prompt.md"
    prompt.write_text("IN_ROOT_PROMPT_MARKER")
    (workspace / "daimonos.toml").write_text(
        "[prompts]\nmcp_instructions = " + json.dumps(str(prompt)) + "\n"
    )
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    process = subprocess.Popen(
        [daimonos_binary, "--mcp", "--workspace", str(workspace),
         "--state-dir", str(state), "--confine-paths"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        client = DaimonosClient(process, str(workspace))
        response = client.send_raw({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "pytest", "version": "1"},
        }})
        assert "IN_ROOT_PROMPT_MARKER" in json.dumps(response)
    finally:
        if process.poll() is None:
            _terminate(process)


def test_confined_cwd_rechecks_replaced_directory(confined, tmp_path):
    client, workspace, _ = confined
    sub = workspace / "sub"
    sub.mkdir()
    (sub / "inside.txt").write_text("inside")
    changed = client.call_tool("set_cwd", {"path": "sub"})
    assert not changed.get("isError"), changed
    moved = tmp_path / "moved"
    sub.rename(moved)
    (moved / "secret.txt").write_text("OUTSIDE_SENTINEL")
    sub.symlink_to(moved, target_is_directory=True)
    refused = client.call_tool("read_file", {"path": "secret.txt"})
    assert refused.get("isError") is True
    assert "OUTSIDE_SENTINEL" not in json.dumps(refused)


def test_explicit_confine_root_enables_confinement(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside.txt"
    outside.write_text("OUTSIDE_SENTINEL")
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(workspace, args=["--state-dir", str(state), "--confine-root", str(workspace)])
    result = client.call_tool("read_file", {"path": str(outside)})
    assert result.get("isError") is True
    assert "path_outside_root" in json.dumps(result)
    assert "OUTSIDE_SENTINEL" not in json.dumps(result)


def test_confined_fifo_read_is_refused_without_blocking(confined):
    import os
    import threading

    client, workspace, _ = confined
    os.mkfifo(workspace / "pipe")
    box = {}
    worker = threading.Thread(
        target=lambda: box.setdefault("result", client.call_tool("read_file", {"path": "pipe"}))
    )
    worker.start()
    worker.join(15)
    assert not worker.is_alive(), "read_file on a FIFO blocked"
    result = box["result"]
    assert result.get("isError") is True, result
    assert "unsupported_file_type" in json.dumps(result)


def _native_requests(daimonos_binary, workspace, requests):
    """Send native-protocol requests to a confined `--socket` server."""
    import socket
    import subprocess
    import tempfile
    import time
    from pathlib import Path

    responses = {}
    # A short directory keeps the socket path under the Unix length limit.
    with tempfile.TemporaryDirectory(prefix="dn-", dir="/tmp") as short:
        state = Path(short) / "s"
        state.mkdir(mode=0o700)
        socket_path = state / "native.sock"
        process = subprocess.Popen(
            [daimonos_binary, "--workspace", str(workspace), "--state-dir", str(state),
             "--confine-paths", "--socket", str(socket_path)],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 10
            while not socket_path.exists() and time.monotonic() < deadline:
                assert process.poll() is None, process.stderr.read().decode(errors="replace")
                time.sleep(0.01)
            assert socket_path.exists()
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(socket_path))
                with client.makefile("rwb") as stream:
                    for name, request in requests.items():
                        stream.write((json.dumps(request) + "\n").encode())
                        stream.flush()
                        responses[name] = json.loads(stream.readline())
        finally:
            if process.poll() is None:
                process.terminate()
            process.communicate(timeout=5)
    return responses


def test_confined_native_socket_refuses_mutating_opcodes(daimonos_binary, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    requests = {
        "write": {"c": 1, "p": "new.txt", "s": "x"},
        "append": {"c": 27, "p": "new.txt", "s": "x"},
        "exec": {"c": 8, "s": "touch pwned"},
        "bg": {"c": 9, "s": "touch pwned_bg"},
        "git": {"c": 15, "s": "status"},
        "snap": {"c": 12},
        "tool_run": {"c": 20, "p": "anything"},
    }
    responses = _native_requests(daimonos_binary, workspace, requests)
    for name, response in responses.items():
        assert response["ok"] is False, (name, response)
        assert response["d"]["code"] == "filesystem_policy_unsupported", (name, response)
    assert list(workspace.iterdir()) == []


def test_confined_glob_parent_prefix_resolves_physically_and_refuses_escape(
    daimonos_binary, tmp_path
):
    workspace = tmp_path / "workspace"
    (workspace / "sub" / "deep").mkdir(parents=True)
    (workspace / "sub" / "side.txt").write_text("side")
    (workspace / "top.txt").write_text("top")
    (workspace / "alias").symlink_to("sub/deep", target_is_directory=True)
    (tmp_path / "outside.txt").write_text("OUTSIDE_SENTINEL")
    requests = {
        "wildcard_through_link": {"c": 5, "p": "alias/../*.txt"},
        "literal_through_link": {"c": 5, "p": "alias/../side.txt"},
        "wildcard_escape": {"c": 5, "p": "../*.txt"},
        "wildcard_escape_through_link": {"c": 5, "p": "alias/../../../*.txt"},
        "literal_escape_through_link": {"c": 5, "p": "alias/../../../outside.txt"},
    }
    responses = _native_requests(daimonos_binary, workspace, requests)
    # `alias/..` is the physical parent of sub/deep (sub), not the workspace
    # root: only sub/side.txt matches, never top.txt. Matches keep the caller's
    # prefix form, which later reads resolve through the same authority.
    for name in ["wildcard_through_link", "literal_through_link"]:
        assert responses[name]["ok"] is True, (name, responses[name])
        assert responses[name]["d"]["files"] == ["alias/../side.txt"], (name, responses[name])
    for name in ["wildcard_escape", "wildcard_escape_through_link", "literal_escape_through_link"]:
        assert responses[name]["ok"] is False, (name, responses[name])
        assert responses[name]["d"]["code"] == "path_outside_root", (name, responses[name])
    assert "OUTSIDE_SENTINEL" not in json.dumps(responses)
