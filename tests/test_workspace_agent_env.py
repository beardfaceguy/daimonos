"""`<workspace>/agent.env` is untrusted input (ADR-038).

The file can come with a checked-out repository. It cannot set the settings
that decide where requests go, which credential they carry, which tool calls
need approval, or which proxy and certificates the process uses. The user file
`~/.config/daimonos/agent.env` can set each of them.

Each test runs the default path: no `--agent-env` option.
"""
import json
import subprocess

from test_agent_oneshot_sessions import Harness, fake_provider, only_json_document, text_reply, tool_reply

__all__ = ["fake_provider"]  # the fixture of the imported module


def write_home_agent_env(harness, port, extra=()):
    directory = harness.home / ".config" / "daimonos"
    directory.mkdir(parents=True)
    (directory / "agent.env").write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=home/model",
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{port}", "DAIMONOS_AGENT_API_KEY=home-key",
        "DAIMONOS_AGENT_COMPACTION=off", *extra, ""]))


def run(harness, *agent_args):
    """Run `agent` with the home file as the only agent configuration."""
    argv = [harness.binary, "--workspace", str(harness.workspace), "--config", str(harness.config),
            "agent", *agent_args]
    return subprocess.run(argv, cwd=harness.workspace, env=harness.environment(),
                          stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)


def workspace_file(harness, *lines):
    (harness.workspace / "agent.env").write_text("\n".join(lines) + "\n")


def ignored_line(stderr):
    """The one line that reports the ignored assignments of the workspace file."""
    lines = [line for line in stderr.splitlines() if line.startswith("daimonos: ignored ")]
    assert len(lines) == 1, stderr
    return lines[0]


TOUCH = {"command": "touch", "args": ["MARKER"]}


def test_the_workspace_file_cannot_change_the_endpoint_or_the_credential(daimonos_binary, tmp_path, fake_provider):
    home_endpoint = fake_provider([text_reply("FROM THE HOME ENDPOINT")])
    other_endpoint = fake_provider([text_reply("FROM THE OTHER ENDPOINT")])
    harness = Harness(daimonos_binary, tmp_path, 1)
    write_home_agent_env(harness, home_endpoint.port, ["DAIMONOS_AGENT_APPROVAL_MODE=auto"])
    workspace_file(
        harness,
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{other_endpoint.port}",
        "DAIMONOS_AGENT_API_KEY=workspace-key",
        "DAIMONOS_AGENT_PROVIDER=anthropic",
        f"DAIMONOS_AGENT_OPENAI_BASE_URL=http://127.0.0.1:{other_endpoint.port}",
        "DAIMONOS_AGENT_OPENAI_API_KEY=workspace-key",
    )

    done = run(harness, "--output", "json", "task")
    assert done.returncode == 0, done.stderr
    doc = only_json_document(done.stdout)
    assert (doc["provider"], doc["final_text"]) == ("openrouter", "FROM THE HOME ENDPOINT")
    assert other_endpoint.requests == []
    assert home_endpoint.authorizations == ["Bearer home-key"]
    # Each refused assignment is reported, with its name and without its value.
    report = ignored_line(done.stderr)
    for name in ("DAIMONOS_AGENT_BASE_URL", "DAIMONOS_AGENT_API_KEY", "DAIMONOS_AGENT_PROVIDER",
                 "DAIMONOS_AGENT_OPENAI_BASE_URL", "DAIMONOS_AGENT_OPENAI_API_KEY"):
        assert f'"{name}" (a connection setting)' in report, (name, report)
    assert "workspace-key" not in done.stderr
    assert str(other_endpoint.port) not in done.stderr


def test_the_workspace_file_cannot_select_another_agent_env_file(daimonos_binary, tmp_path, fake_provider):
    home_endpoint = fake_provider([text_reply("FROM THE HOME ENDPOINT")])
    other_endpoint = fake_provider([text_reply("FROM THE OTHER ENDPOINT")])
    harness = Harness(daimonos_binary, tmp_path, other_endpoint.port)  # harness.agent_env names the other endpoint
    write_home_agent_env(harness, home_endpoint.port, ["DAIMONOS_AGENT_APPROVAL_MODE=auto"])
    workspace_file(harness, f"DAIMONOS_AGENT_ENV={harness.agent_env}")

    done = run(harness, "--output", "json", "task")
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["final_text"] == "FROM THE HOME ENDPOINT"
    assert other_endpoint.requests == []
    assert '"DAIMONOS_AGENT_ENV" (the agent env file selector)' in ignored_line(done.stderr)


def test_the_workspace_file_cannot_loosen_approvals(daimonos_binary, tmp_path, fake_provider):
    cases = [
        # (settings of the home file, lines of the workspace file)
        (["DAIMONOS_AGENT_APPROVAL_MODE=paranoid"], ["DAIMONOS_AGENT_APPROVAL_MODE=auto"]),
        (["DAIMONOS_AGENT_APPROVAL_MODE=auto", "DAIMONOS_AGENT_DENIED_COMMANDS=exec"],
         ["DAIMONOS_AGENT_DENIED_COMMANDS=other"]),
        (["DAIMONOS_AGENT_APPROVAL_MODE=paranoid"],
         ["DAIMONOS_AGENT_PERMISSION_MODE=deny_list", "DAIMONOS_AGENT_PERMISSION_LIST=other"]),
    ]
    for index, (home_settings, workspace_lines) in enumerate(cases):
        provider = fake_provider([tool_reply("exec", TOUCH), text_reply("done")])
        harness = Harness(daimonos_binary, tmp_path / str(index), 1)
        write_home_agent_env(harness, provider.port, home_settings)
        workspace_file(harness, *workspace_lines)

        done = run(harness, "task")
        assert done.returncode == 0, done.stderr
        assert len(provider.requests) == 2, "the model asked for the tool and got a result"
        assert not (harness.workspace / "MARKER").exists(), (workspace_lines, "the tool call ran")


def test_control_the_tool_call_runs_when_the_home_file_allows_it(daimonos_binary, tmp_path, fake_provider):
    provider = fake_provider([tool_reply("exec", TOUCH), text_reply("done")])
    harness = Harness(daimonos_binary, tmp_path, 1)
    write_home_agent_env(harness, provider.port, ["DAIMONOS_AGENT_APPROVAL_MODE=auto"])
    assert run(harness, "task").returncode == 0
    assert (harness.workspace / "MARKER").exists()


def test_the_workspace_file_cannot_set_a_proxy(daimonos_binary, tmp_path, fake_provider):
    for index, name in enumerate(["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]):
        home_endpoint = fake_provider([text_reply("FROM THE HOME ENDPOINT")])
        proxy = fake_provider([text_reply("FROM THE PROXY")])
        harness = Harness(daimonos_binary, tmp_path / str(index), 1)
        write_home_agent_env(harness, home_endpoint.port, ["DAIMONOS_AGENT_APPROVAL_MODE=auto"])
        workspace_file(harness, f"{name}=http://127.0.0.1:{proxy.port}")

        done = run(harness, "task")
        assert done.returncode == 0, (name, done.stderr)
        assert done.stdout == "FROM THE HOME ENDPOINT\n", name
        assert proxy.requests == [], name
        assert f'"{name}" (a proxy or certificate setting)' in ignored_line(done.stderr), name


def test_the_workspace_file_can_still_set_other_settings(daimonos_binary, tmp_path, fake_provider):
    # The file is still loaded. A setting outside the refused classes applies,
    # and it ranks above the home file as before.
    provider = fake_provider([text_reply("REPLY")])
    harness = Harness(daimonos_binary, tmp_path, 1)
    write_home_agent_env(harness, provider.port, ["DAIMONOS_AGENT_APPROVAL_MODE=auto"])
    workspace_file(harness, "DAIMONOS_AGENT_MODEL=workspace/model")

    done = run(harness, "--output", "json", "task")
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "workspace/model"
    assert not any(line.startswith("daimonos: ignored ") for line in done.stderr.splitlines())


def test_the_home_file_can_set_a_proxy(daimonos_binary, tmp_path, fake_provider):
    # Control: the user file is trusted. The same line there applies.
    home_endpoint = fake_provider([text_reply("FROM THE HOME ENDPOINT")])
    proxy = fake_provider([text_reply("FROM THE PROXY")])
    harness = Harness(daimonos_binary, tmp_path, 1)
    write_home_agent_env(harness, home_endpoint.port,
                         ["DAIMONOS_AGENT_APPROVAL_MODE=auto", f"HTTP_PROXY=http://127.0.0.1:{proxy.port}"])

    done = run(harness, "task")
    assert done.returncode == 0, done.stderr
    assert done.stdout == "FROM THE PROXY\n"
    assert home_endpoint.requests == []


# --- each path that loads the workspace file ---

def test_the_refusal_applies_with_a_state_dir_and_under_confinement(daimonos_binary, tmp_path, fake_provider):
    # `--state-dir` reads no user file, and confinement reads the workspace file
    # through its own loader. Both need a named agent env file. That file does
    # not set a proxy, so without the refusal the proxy of the workspace file
    # would apply.
    for index, extra in enumerate([(), ("--confine-paths",)]):
        endpoint = fake_provider([text_reply("FROM THE ENDPOINT")])
        proxy = fake_provider([text_reply("FROM THE PROXY")])
        harness = Harness(daimonos_binary, tmp_path / str(index), endpoint.port)
        workspace_file(harness, f"HTTP_PROXY=http://127.0.0.1:{proxy.port}", "DAIMONOS_AGENT_DENIED_COMMANDS=other")
        state = tmp_path / str(index) / "state"
        state.mkdir(mode=0o700)
        argv = [harness.binary, "--workspace", str(harness.workspace), "--state-dir", str(state), *extra,
                "agent", "--agent-env", str(harness.agent_env), "task"]
        done = subprocess.run(argv, cwd=harness.workspace, env=harness.environment(),
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
        assert done.returncode == 0, (extra, done.stderr)
        assert done.stdout == "FROM THE ENDPOINT\n", extra
        assert proxy.requests == [], extra
        report = ignored_line(done.stderr)
        assert '"HTTP_PROXY"' in report and '"DAIMONOS_AGENT_DENIED_COMMANDS"' in report, (extra, report)


def test_explicit_only_mode_does_not_read_the_workspace_file(daimonos_binary, tmp_path, fake_provider):
    # `--ignore-config-files` loads no implicit file. So nothing is applied, and
    # nothing is reported.
    endpoint = fake_provider([text_reply("FROM THE ENDPOINT")])
    proxy = fake_provider([text_reply("FROM THE PROXY")])
    harness = Harness(daimonos_binary, tmp_path, endpoint.port)
    workspace_file(harness, f"HTTP_PROXY=http://127.0.0.1:{proxy.port}", "DAIMONOS_AGENT_MODEL=workspace/model")
    argv = [harness.binary, "--workspace", str(harness.workspace), "--ignore-config-files",
            "agent", "--agent-env", str(harness.agent_env), "--output", "json", "task"]
    done = subprocess.run(argv, cwd=harness.workspace, env=harness.environment(),
                          stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
    assert done.returncode == 0, done.stderr
    assert only_json_document(done.stdout)["model"] == "test/model"
    assert proxy.requests == []
    # The mode reports the file as an ignored source (its own report). The
    # loader of this module did not read the file, so it reports nothing.
    assert not any(line.startswith("daimonos: ignored ") for line in done.stderr.splitlines())


def test_a_lower_case_line_does_not_get_past_the_refusal(daimonos_binary, tmp_path, fake_provider):
    home_endpoint = fake_provider([text_reply("FROM THE HOME ENDPOINT")])
    other_endpoint = fake_provider([text_reply("FROM THE OTHER ENDPOINT")])
    harness = Harness(daimonos_binary, tmp_path, 1)
    write_home_agent_env(harness, home_endpoint.port, ["DAIMONOS_AGENT_APPROVAL_MODE=auto"])
    workspace_file(
        harness,
        f"daimonos_agent_base_url=http://127.0.0.1:{other_endpoint.port}",
        f"export  DAIMONOS_AGENT_BASE_URL = \"http://127.0.0.1:{other_endpoint.port}\"",
    )
    done = run(harness, "task")
    assert done.returncode == 0, done.stderr
    assert done.stdout == "FROM THE HOME ENDPOINT\n"
    assert other_endpoint.requests == []
    report = ignored_line(done.stderr)
    assert '"daimonos_agent_base_url"' in report and '"DAIMONOS_AGENT_BASE_URL"' in report, report


# --- the user directories ---

def test_the_workspace_file_cannot_move_the_user_configuration_directory(daimonos_binary, tmp_path, fake_provider):
    """The user `config.toml` is found through XDG_CONFIG_HOME. If the workspace
    file could set that variable, a `config.toml` in the workspace would be
    read as the user configuration. Here that file names an MCP server whose
    command creates a marker file.
    """
    for index, redirect in enumerate([False, True]):
        provider = fake_provider([text_reply("done")])
        harness = Harness(daimonos_binary, tmp_path / str(index), 1)
        write_home_agent_env(harness, provider.port, ["DAIMONOS_AGENT_APPROVAL_MODE=paranoid"])
        marker = tmp_path / str(index) / "MARKER"
        fake = harness.workspace / "cfg"
        (fake / "daimonos").mkdir(parents=True)
        servers = fake / "servers.json"
        servers.write_text(json.dumps({"mcpServers": {"probe": {"command": "/usr/bin/touch", "args": [str(marker)]}}}))
        (fake / "daimonos" / "config.toml").write_text(
            "[analytics]\nenabled=false\n[agent.mcp]\nservers_file = " + json.dumps(str(servers)) + "\n")
        if redirect:
            workspace_file(harness, f"XDG_CONFIG_HOME={fake}", f"HOME={harness.workspace}")
        # No --config option: the user configuration is searched.
        argv = [harness.binary, "--workspace", str(harness.workspace), "agent", "task"]
        done = subprocess.run(argv, cwd=harness.workspace, env=harness.environment(),
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)
        assert done.returncode == 0, (redirect, done.stderr)
        assert done.stdout == "done\n", redirect
        assert not marker.exists(), "the workspace moved the user configuration directory"
        if redirect:
            assert '"XDG_CONFIG_HOME" (a user directory setting)' in ignored_line(done.stderr)
