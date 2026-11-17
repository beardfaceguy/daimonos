"""Explicit-only configuration isolation (WP11, FR-09, ADR-021, AC-09.1/09.2).

Context: docs/contexts/reviewer-runtime/CONTEXT.md. The ``--ignore-config-files``
run option refuses every implicit configuration source (D01-D09 in
reviewer-runtime INVENTORIES.md) so a hostile workspace or home file cannot
configure an opted-in run, while an explicit ``--config`` the caller passes is
still honoured. Each ignored source is reported at startup, by path, without its
contents being read. The option is default-off: current behaviour is unchanged.

These tests drive a real ``daimonos`` MCP process over stdio.
"""

from __future__ import annotations

import json

IGNORE_FLAG = "--ignore-config-files"

# Default-off SearchConfig::default().default_grep_max is 100; the hostile
# workspace config narrows it to 1. The gap is the observable: a loaded config
# caps `search` at one match; an ignored config leaves the built-in default so
# every seeded match is returned.
SEEDED_MATCHES = 5
NARROW_MAX = 1
PATTERN = "NEEDLE"


def _seed_matches(workspace, count=SEEDED_MATCHES):
    for i in range(count):
        (workspace / f"f{i}.txt").write_text(f"{PATTERN} line\n")


def _grep_count(client):
    result = client.call_tool("search", {"pattern": PATTERN, "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    return len(payload.get("matches", []))


def _drain_stderr(client):
    """Terminate the process and return its full captured stderr text."""
    proc = client.process
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()
        proc.wait()
    return proc.stderr.read().decode(errors="replace")


def _drain_effective_policy(client):
    """Terminate the process and parse the effective-policy startup report."""
    for line in _drain_stderr(client).splitlines():
        line = line.strip()
        if '"effective_policy"' in line:
            return json.loads(line)["effective_policy"]
    return None


def test_optout_workspace_config_is_applied(daimonos_factory, tmp_path):
    """Default-off: a workspace daimonos.toml still configures the run."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _seed_matches(ws)
    (ws / "daimonos.toml").write_text(f"[search]\ndefault_grep_max = {NARROW_MAX}\n")
    client = daimonos_factory(ws)
    assert _grep_count(client) == NARROW_MAX


def test_ignore_config_files_ignores_workspace_config(daimonos_factory, tmp_path):
    """AC-09.1 (config kind): the workspace daimonos.toml changes nothing and is
    reported as ignored by path."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _seed_matches(ws)
    (ws / "daimonos.toml").write_text(f"[search]\ndefault_grep_max = {NARROW_MAX}\n")
    client = daimonos_factory(ws, args=[IGNORE_FLAG])
    assert _grep_count(client) == SEEDED_MATCHES
    policy = _drain_effective_policy(client)
    assert policy is not None, "explicit-only run must print an effective-policy report"
    assert policy["ignore_config_files"] is True
    ignored = policy["ignored"]
    assert any(
        entry["path"].endswith("daimonos.toml") and entry["kind"] == "D02"
        for entry in ignored
    ), ignored


def test_explicit_config_still_honored(daimonos_factory, tmp_path):
    """AC-09.2: an explicit --config the caller passes is honoured even under
    explicit-only mode."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _seed_matches(ws)
    explicit = tmp_path / "explicit.toml"
    explicit.write_text(f"[search]\ndefault_grep_max = {NARROW_MAX}\n")
    client = daimonos_factory(ws, args=[IGNORE_FLAG, "--config", str(explicit)])
    assert _grep_count(client) == NARROW_MAX


def _search_files(client, pattern=PATTERN):
    result = client.call_tool("search", {"pattern": pattern, "mode": "content"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    return {match["f"] for match in payload.get("matches", [])}


def _seed_ignored_tree(workspace):
    # `.ignore` is always honoured by the walk (independent of a git repo).
    (workspace / ".ignore").write_text("hidden.txt\n")
    (workspace / "hidden.txt").write_text(f"{PATTERN} in hidden\n")
    (workspace / "visible.txt").write_text(f"{PATTERN} in visible\n")


def test_optout_ignore_file_still_applies_in_search(daimonos_factory, tmp_path):
    """Default-off: an implicit .ignore file still steers search (unchanged)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _seed_ignored_tree(ws)
    files = _search_files(daimonos_factory(ws))
    assert any(f.endswith("visible.txt") for f in files), files
    assert not any(f.endswith("hidden.txt") for f in files), files


def test_ignore_config_files_disables_ignore_files_in_search(daimonos_factory, tmp_path):
    """AC-09.1 (ignore-file kind): an implicit .ignore file does not alter tool
    behaviour under explicit-only mode, and is reported as ignored."""
    ws = tmp_path / "ws"
    ws.mkdir()
    _seed_ignored_tree(ws)
    client = daimonos_factory(ws, args=[IGNORE_FLAG])
    files = _search_files(client)
    assert any(f.endswith("visible.txt") for f in files), files
    assert any(f.endswith("hidden.txt") for f in files), files
    policy = _drain_effective_policy(client)
    assert any(
        entry["kind"] == "D08" and entry["path"].endswith(".ignore")
        for entry in policy["ignored"]
    ), policy


def test_env_file_is_ignored_but_applied_by_default(daimonos_factory, tmp_path):
    """AC-09.1 (environment-file kind, D01): an implicit ``agent.env`` is loaded
    into the daimonos process environment by default (observable in the startup
    load summary) but under explicit-only mode it is neither loaded nor read -
    only reported as an ignored source by path.

    ``agent.env`` variables configure daimonos itself (e.g. API keys read via
    ``std::env::var``); they are deliberately NOT injected into exec'd children
    (``managed_process`` clears the child environment, seeding only ``PATH`` -
    see ``src/ops/exec_ops.rs`` + ``src/session.rs``). So the D01 load/skip
    decision is observed through the startup summary and the effective-policy
    report, not through a child process's ``$VAR``.
    """
    def seed(name):
        ws = tmp_path / name
        ws.mkdir()
        (ws / "agent.env").write_text("WP11_ENV_SENTINEL=leaked\n")
        return ws

    # Default (opt-out regression): the workspace agent.env is loaded; the
    # startup summary names it by path.
    default_ws = seed("default")
    default_stderr = _drain_stderr(daimonos_factory(default_ws, args=["--verbose"]))
    assert any(
        "loaded" in line and line.rstrip().endswith("default/agent.env")
        for line in default_stderr.splitlines()
    ), default_stderr

    # Explicit-only: agent.env is never read (no load summary for it) and is
    # reported ignored as D01, by absolute path.
    explicit_ws = seed("explicit")
    client = daimonos_factory(explicit_ws, args=["--verbose", IGNORE_FLAG])
    policy_stderr = _drain_stderr(client)
    assert not any(
        "loaded" in line and line.rstrip().endswith("explicit/agent.env")
        for line in policy_stderr.splitlines()
    ), policy_stderr
    policy = None
    for line in policy_stderr.splitlines():
        line = line.strip()
        if '"effective_policy"' in line:
            policy = json.loads(line)["effective_policy"]
            break
    assert policy is not None, "explicit-only run must print an effective-policy report"
    assert any(
        entry["kind"] == "D01"
        and entry["path"].endswith("explicit/agent.env")
        and entry["path"].startswith("/")
        for entry in policy["ignored"]
    ), policy


def test_mixed_hostile_corpus_is_fully_ignored_and_reported(daimonos_factory, tmp_path):
    """AC-09.1: a workspace carrying every recognised configuration kind changes
    nothing about the run, and each ignored source is reported by path."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (ws / "agent.env").write_text("WP11_ENV_SENTINEL=leaked\n")
    (ws / "daimonos.toml").write_text(f"[search]\ndefault_grep_max = {NARROW_MAX}\n")
    (ws / "agent-instructions.md").write_text("HOSTILE INSTRUCTIONS\n")
    (ws / ".ignore").write_text("hidden.txt\n")
    skill = ws / ".agents" / "skills" / "demo"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: demo\ndescription: hostile\n---\nbody\n")
    _seed_matches(ws)
    (ws / "hidden.txt").write_text(f"{PATTERN} hidden\n")

    client = daimonos_factory(ws, args=[IGNORE_FLAG])
    files = _search_files(client)
    # config narrowing not applied (default max) and ignore file not applied.
    assert any(f.endswith("hidden.txt") for f in files), files
    assert len(files) >= SEEDED_MATCHES, files

    policy = _drain_effective_policy(client)
    assert policy["ignore_config_files"] is True
    kinds = {entry["kind"] for entry in policy["ignored"]}
    for code in ("D01", "D02", "D04", "D05", "D08"):
        assert code in kinds, policy
    # The report names paths and never reads contents; every path is absolute.
    assert all(entry["path"].startswith("/") for entry in policy["ignored"]), policy
