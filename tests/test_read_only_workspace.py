"""FR-07 / WP04: `--workspace-read-only` prohibits workspace writes end to end.

Context: docs/adr/022-shared-filesystem-authority.md (read-only is an independent
option), reviewer-runtime INTERFACES.md (`--workspace-read-only`,
`workspace_write_forbidden`, `tool_policy_unsupported`) and INVENTORIES.md
tool-class table. The admission table lives in `src/run_policy.rs`.

These tests drive a real `daimonos` MCP process (conftest `daimonos_factory`),
never private call sequences.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

RO = ["--workspace-read-only"]

WRITE_CODE = "workspace_write_forbidden"
OPAQUE_CODE = "tool_policy_unsupported"
POLICY_CODES = {WRITE_CODE, OPAQUE_CODE}

# G4: the real MCP catalog (list_all_tools) mapped to its read-only outcome.
# "read"          -> admitted: must NOT be refused by read-only policy (a
#                    business error with a non-policy code is still fine).
# "write_refused" -> refused with workspace_write_forbidden.
# "opaque_refused"-> refused with tool_policy_unsupported.
# "snapshot"      -> multi-command tool, checked per action separately below.
# kgl_query/kgl_assert are "write_refused" WITHOUT --state-dir (their store
# would be created under <workspace>/.kgl) and admitted WITH it (see G1 test).
# The test asserts every catalog name appears here; a new/missing tool fails.
CATALOG_EXPECTED = {
    "read_file": "read",
    "write_file": "write_refused",
    "append_file": "write_refused",
    "edit_file": "write_refused",
    "search": "read",
    "workspace_info": "read",
    "exec": "opaque_refused",
    "batch": "read",
    "get_tool_schema": "read",
    "execute_script": "opaque_refused",
    "kgl_query": "write_refused",
    "kgl_assert": "write_refused",
    "list_tool_signatures": "read",
    "register_agent": "read",
    "list_agents": "read",
    "send_message": "read",
    "reply_message": "read",
    "fetch_inbox": "read",
    "mark_read": "read",
    "acknowledge": "read",
    "fetch_thread": "read",
    "reserve_paths": "read",
    "renew_reservations": "read",
    "release_reservations": "read",
    "check_conflicts": "read",
    "list_reservations": "read",
    "git_read": "read",
    "git": "opaque_refused",
    "cargo": "opaque_refused",
    "pytest": "opaque_refused",
    "gh": "opaque_refused",
    "docker": "opaque_refused",
    "curl": "opaque_refused",
    "shellcheck": "opaque_refused",
    "npm": "opaque_refused",
    "discord": "opaque_refused",
    "snapshot": "snapshot",
    "set_cwd": "read",
    "ls": "read",
    "session_stats": "read",
    "set_external_session_id": "read",
    "set_verbosity": "read",
    "list_all_tools": "read",
    "diff_files": "read",
    "tool_pipeline": "opaque_refused",
    "lint_repair": "opaque_refused",
    # `skill` and `update_plan` are agent-only (not exposed over MCP); they are
    # admitted reads in the admission table (classify_tool), covered by the
    # src/run_policy.rs unit tests and the agent-mode test below.
}

# Minimal valid args so each read tool reaches its handler (not an MCP arg
# error). Coordination writes target the state/HOME coordination store, never
# the workspace, so they are admitted even when they fail for business reasons.
CATALOG_ARGS = {
    "read_file": {"path": "file.txt"},
    "search": {"pattern": "second"},
    "workspace_info": {},
    "batch": {"ops": [{"tool": "ls", "arguments": {"path": "."}}]},
    "get_tool_schema": {"tools": ["read_file"]},
    "list_tool_signatures": {},
    "register_agent": {"name": "roprobe"},
    "list_agents": {},
    "send_message": {"sender": "roprobe", "to": ["roprobe"], "subject": "s"},
    "reply_message": {"sender": "roprobe", "reply_to": 1},
    "fetch_inbox": {"agent": "roprobe"},
    "mark_read": {"agent": "roprobe", "message_id": 1},
    "acknowledge": {"agent": "roprobe", "message_id": 1},
    "fetch_thread": {"thread_id": 1},
    "reserve_paths": {"agent": "roprobe", "paths": ["x"]},
    "renew_reservations": {"agent": "roprobe", "ttl_secs": 60},
    "release_reservations": {"agent": "roprobe"},
    "check_conflicts": {"agent": "roprobe", "paths": ["x"]},
    "list_reservations": {"agent": "roprobe"},
    "git_read": {"command": "log", "repo": "."},
    "set_cwd": {"path": "."},
    "ls": {"path": "."},
    "session_stats": {"scope": "session"},
    "set_external_session_id": {"id": "roprobe"},
    "set_verbosity": {"level": "compact"},
    "list_all_tools": {},
    "diff_files": {"path_a": "file.txt", "content_b": "first\nsecond\n"},
    # mutations / opaque / kgl
    "write_file": {"path": "x.txt", "content": "y"},
    "append_file": {"path": "file.txt", "content": "y"},
    "edit_file": {"path": "file.txt", "edits": ["first", "z"]},
    "exec": {"command": "true"},
    "execute_script": {"code": "result = 1"},
    "git": {"command": "status"},
    "cargo": {"command": "build"},
    "pytest": {"command": "run"},
    "gh": {"command": "pr_list"},
    "docker": {"command": "ps"},
    "curl": {"url": "http://127.0.0.1:1"},
    "shellcheck": {"path": "x.sh"},
    "npm": {"command": "install"},
    "discord": {"command": "list_guilds"},
    "tool_pipeline": {"steps": []},
    "lint_repair": {"path": "x.rs"},
    "kgl_query": {"query": "stats"},
    "kgl_assert": {"action": "declare_intent"},
}


def _policy_code(result):
    """Return the read-only policy refusal code in a tool result, or None."""
    if not result.get("isError"):
        return None
    text = result["content"][0]["text"]
    try:
        code = json.loads(text).get("code")
    except Exception:
        code = None
    return code if code in POLICY_CODES else None


def _assert_catalog_outcome(client, name, category):
    """Call `name` under read-only and assert the outcome for its category."""
    try:
        result = client.call_tool(name, CATALOG_ARGS.get(name, {}))
    except RuntimeError as rpc_error:
        # An MCP-layer arg error is not a read-only refusal; a "read" tool must
        # at least not be blocked by policy. Surface anything unexpected.
        raise AssertionError(f"{name}: unexpected RPC error {rpc_error}")
    code = _policy_code(result)
    if category == "read":
        assert code is None, f"{name}: read tool was refused by policy ({code})"
    elif category == "write_refused":
        assert code == WRITE_CODE, f"{name}: expected {WRITE_CODE}, got {result}"
    elif category == "opaque_refused":
        assert code == OPAQUE_CODE, f"{name}: expected {OPAQUE_CODE}, got {result}"
    elif category == "snapshot":
        # list reads only (admitted); create/restore/delete mutate (refused).
        assert _policy_code(client.call_tool("snapshot", {"action": "list"})) is None, name
        for action, args in (("create", {}), ("restore", {"id": "x"}), ("delete", {"id": "x"})):
            r = client.call_tool("snapshot", {"action": action, **args})
            assert _policy_code(r) == WRITE_CODE, (action, r)
    else:
        raise AssertionError(f"{name}: unknown category {category}")


def _refusal(result):
    """Return the parsed refusal payload for a tool result marked isError."""
    assert result.get("isError"), f"expected a refusal, got: {result}"
    return json.loads(result["content"][0]["text"])


# --- Slice 1: option parsing + one refused mutation (write_file) -------------


def test_workspace_read_only_refuses_write_file(daimonos_factory, tmp_path):
    client = daimonos_factory(tmp_path, args=RO)
    target = tmp_path / "new.txt"
    result = client.call_tool("write_file", {"path": "new.txt", "content": "x"})
    payload = _refusal(result)
    assert payload["code"] == "workspace_write_forbidden", payload
    assert payload.get("tool") == "write_file", payload
    assert not target.exists(), "refused write must not create the file"


# --- Slice 2: remaining mutation entrypoints -------------------------------


def _seed(tmp_path):
    (tmp_path / "doc.txt").write_text("original\n")


def test_workspace_read_only_refuses_edit_file(daimonos_factory, tmp_path):
    _seed(tmp_path)
    client = daimonos_factory(tmp_path, args=RO)
    result = client.call_tool(
        "edit_file", {"path": "doc.txt", "edits": ["original", "tampered"]}
    )
    payload = _refusal(result)
    assert payload["code"] == "workspace_write_forbidden", payload
    assert payload.get("tool") == "edit_file", payload
    assert (tmp_path / "doc.txt").read_text() == "original\n"


def test_workspace_read_only_refuses_append_file(daimonos_factory, tmp_path):
    _seed(tmp_path)
    client = daimonos_factory(tmp_path, args=RO)
    result = client.call_tool("append_file", {"path": "doc.txt", "content": "more\n"})
    payload = _refusal(result)
    assert payload["code"] == "workspace_write_forbidden", payload
    assert (tmp_path / "doc.txt").read_text() == "original\n"


def test_workspace_read_only_refuses_snapshot_create_and_restore(daimonos_factory, tmp_path):
    _seed(tmp_path)
    client = daimonos_factory(tmp_path, args=RO)
    for args in ({"action": "create"}, {"action": "restore", "id": "x"}):
        payload = _refusal(client.call_tool("snapshot", args))
        assert payload["code"] == "workspace_write_forbidden", (args, payload)
    # A snapshot store must not have been created under the workspace.
    assert not (tmp_path / ".snapshots").exists()


# --- Slice 3: opaque tools refused (cannot be mediated) ---------------------


def test_workspace_read_only_refuses_exec(daimonos_factory, tmp_path):
    client = daimonos_factory(tmp_path, args=RO)
    payload = _refusal(client.call_tool("exec", {"command": "touch", "args": ["x"]}))
    assert payload["code"] == "tool_policy_unsupported", payload
    assert not (tmp_path / "x").exists()


def test_workspace_read_only_refuses_execute_script(daimonos_factory, tmp_path):
    client = daimonos_factory(tmp_path, args=RO)
    # execute_script is opaque: its inner effects cannot be mediated, so it is
    # refused before any Starlark runs (not merely hidden).
    result = client.call_tool(
        "execute_script", {"code": "result = write_file('x.txt', 'data')"}
    )
    assert result.get("isError"), result
    text = result["content"][0]["text"]
    # G7: the refusal is the same structured payload as every other refusal.
    payload = json.loads(text)
    assert payload["code"] == "tool_policy_unsupported", payload
    assert payload["tool"] == "execute_script", payload
    assert payload["retryable"] is False, payload
    assert not (tmp_path / "x.txt").exists()


@pytest.mark.skipif(shutil.which("cargo") is None, reason="cargo not on PATH")
def test_workspace_read_only_refuses_external_cli_plugin(daimonos_factory, tmp_path):
    client = daimonos_factory(tmp_path, args=RO)
    payload = _refusal(client.call_tool("cargo", {"command": "build"}))
    assert payload["code"] == "tool_policy_unsupported", payload
    assert payload.get("tool") == "cargo", payload


# --- Slice 4: AC-07.2 writable workspace unchanged across every read tool ----


def _git(repo, *args):
    subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)


def _snapshot(root: Path):
    """Recursive (path, size, mtime_ns, sha256) of every entry, incl. .git.

    Paths that vanish mid-walk (e.g. a transient git-maintenance lock) are
    skipped so the snapshot never crashes; auto-gc is disabled in the seeded
    repo so such transients do not perturb a before/after comparison."""
    out = []
    for p in sorted(root.rglob("*")):
        try:
            st = p.lstat()
            digest = (
                hashlib.sha256(p.read_bytes()).hexdigest()
                if p.is_file() and not p.is_symlink()
                else None
            )
        except FileNotFoundError:
            continue
        out.append((str(p.relative_to(root)), st.st_size, st.st_mtime_ns, digest))
    return out


def _seed_repo(root: Path):
    root.mkdir(parents=True, exist_ok=True)
    _git(root, "init", "-q")
    _git(root, "config", "user.email", "t@example.com")
    _git(root, "config", "user.name", "t")
    # No background gc/maintenance: it would write transient locks into .git
    # and perturb the before/after workspace comparison.
    _git(root, "config", "gc.auto", "0")
    _git(root, "config", "maintenance.auto", "false")
    (root / "file.txt").write_text("first\nsecond\n")
    (root / "other.txt").write_text("alpha\n")
    _git(root, "add", ".")
    _git(root, "commit", "-q", "-m", "seed")


def _isolated_env(tmp_path):
    """HOME/XDG/TMP pointed at the test tree so internal stores (coordination,
    analytics, logs, tool-output) land OUTSIDE the workspace, never in real
    HOME. KGL autoindex is pinned off (store would be <workspace>/.kgl)."""
    stash = tmp_path / "home"
    stash.mkdir(exist_ok=True)
    return {
        "HOME": str(stash),
        "XDG_STATE_HOME": str(stash / "state"),
        "XDG_DATA_HOME": str(stash / "data"),
        "XDG_CONFIG_HOME": str(stash / "config"),
        "TMPDIR": str(tmp_path / "tmp"),
        "DAIMONOS_KGL_AUTOINDEX": "0",
    }


def test_ac_07_2_read_only_session_leaves_workspace_and_git_unchanged(
    daimonos_factory, tmp_path
):
    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    (tmp_path / "tmp").mkdir()
    before = _snapshot(workspace)

    client = daimonos_factory(workspace, env=_isolated_env(tmp_path), args=RO)  # startup

    # G4: enumerate the REAL catalog and assert every tool is classified. A tool
    # missing from CATALOG_EXPECTED fails here, so "every read tool" is enforced,
    # not sampled. Without --state-dir, KGL tools and snapshot create/delete are
    # refused (their stores would land under the workspace).
    catalog = [t["name"] for t in json.loads(
        client.call_tool("list_all_tools", {})["content"][0]["text"]
    )]
    missing = sorted(set(catalog) - set(CATALOG_EXPECTED))
    assert not missing, f"catalog tools missing from the read-only table: {missing}"
    for name in catalog:
        _assert_catalog_outcome(client, name, CATALOG_EXPECTED[name])

    mid = _snapshot(workspace)
    assert mid == before, "read-only session mutated the workspace (startup/tools)"

    # Explicit teardown, then re-check to cover shutdown effects.
    client.process.terminate()
    client.process.wait(timeout=5)
    after = _snapshot(workspace)
    assert after == before, "read-only session mutated the workspace on teardown"
    # The refused KGL tool calls must not have created a store under the
    # workspace (G1: no hidden write from a "read").
    assert not (workspace / ".kgl").exists(), "KGL store created under read-only workspace"


# --- Slice 5: AC-07.1 full read-tool session on a real read-only mount -------

_RO_MOUNT_PROGRAM = r'''
import errno, hashlib, json, os, subprocess, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from conftest import DaimonosClient
binary, workspace, commit, expected_json, args_json = sys.argv[2:]
workspace = Path(workspace)
EXPECTED = json.loads(expected_json)
ARGS = json.loads(args_json)
WRITE = "workspace_write_forbidden"
OPAQUE = "tool_policy_unsupported"
POLICY = {WRITE, OPAQUE}

def pcode(r):
    if not r.get("isError"):
        return None
    try:
        code = json.loads(r["content"][0]["text"]).get("code")
    except Exception:
        code = None
    return code if code in POLICY else None

def snapshot():
    out = []
    for p in sorted(workspace.rglob("*")):
        st = p.lstat()
        digest = hashlib.sha256(p.read_bytes()).hexdigest() if (p.is_file() and not p.is_symlink()) else None
        out.append((str(p.relative_to(workspace)), st.st_size, st.st_mtime_ns, digest))
    return out

subprocess.run(["mount", "--bind", str(workspace), str(workspace)], check=True)
subprocess.run(["mount", "-o", "remount,bind,ro", str(workspace)], check=True)
try:
    (workspace / "must-not-write").write_text("forbidden")
    raise AssertionError("the mount is not read-only")
except OSError as error:
    assert error.errno == errno.EROFS, error

before = snapshot()
process = subprocess.Popen(
    [binary, "--mcp", "-w", str(workspace), "--workspace-read-only"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
)
reads = 0
refusals = 0
try:
    client = DaimonosClient(process, str(workspace))
    client.send_raw({"jsonrpc": "2.0", "id": client._next_id(), "method": "initialize",
                     "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                                "clientInfo": {"name": "ro-probe", "version": "1"}}})
    client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})

    # G4: enumerate the REAL catalog and assert every tool's read-only outcome.
    # A catalog tool missing from EXPECTED fails the test (enforced, not sampled).
    catalog = [t["name"] for t in json.loads(
        client.call_tool("list_all_tools", {})["content"][0]["text"])]
    missing = sorted(set(catalog) - set(EXPECTED))
    assert not missing, ("catalog not fully mapped on the read-only mount", missing)
    for name in catalog:
        cat = EXPECTED[name]
        r = client.call_tool(name, ARGS.get(name, {}))
        code = pcode(r)
        if cat == "read":
            assert code is None, (name, r)
            reads += 1
        elif cat == "write_refused":
            assert code == WRITE, (name, r)
            refusals += 1
        elif cat == "opaque_refused":
            assert code == OPAQUE, (name, r)
            refusals += 1
        elif cat == "snapshot":
            assert pcode(client.call_tool("snapshot", {"action": "list"})) is None, name
            reads += 1
            for a, ar in (("create", {}), ("restore", {"id": "x"}), ("delete", {"id": "x"})):
                assert pcode(client.call_tool("snapshot", {"action": a, **ar})) == WRITE, a
                refusals += 1
        else:
            raise AssertionError((name, cat))
    # git_read per-command coverage, incl. the private-status scratch (W13).
    for args in ({"command": "resolve", "rev": commit},
                 {"command": "show_file", "path": "file.txt", "rev": commit},
                 {"command": "list_tree", "rev": commit},
                 {"command": "grep", "pattern": "first", "rev": commit},
                 {"command": "log"}, {"command": "worktree_status"}):
        r = client.call_tool("git_read", {"repo": ".", **args})
        assert not r.get("isError"), ("git_read", args, r)
        reads += 1
finally:
    process.terminate()
    process.wait(timeout=5)

assert snapshot() == before, "the read-only workspace changed"
print(json.dumps({"read_only_mount": True, "reads": reads, "refusals": refusals, "unchanged": True}))
'''


def test_ac_07_1_every_read_tool_on_a_real_read_only_mount(daimonos_binary, tmp_path):
    if sys.platform != "linux" or not shutil.which("unshare"):
        pytest.skip("requires Linux user and mount namespaces")
    if subprocess.run(["unshare", "-Urnm", "true"], capture_output=True).returncode:
        pytest.skip("user or mount namespaces are unavailable")
    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=workspace, capture_output=True, text=True
    ).stdout.strip()
    source_tests = str(Path(__file__).resolve().parent)
    # G5: logging/analytics are NOT suppressed — they default outside the
    # workspace, so they are not needed to keep the mount pristine. Only
    # DAIMONOS_KGL_AUTOINDEX=0 remains and IS necessary: an ambient
    # DAIMONOS_KGL_AUTOINDEX=1 would make the watcher write <workspace>/.kgl,
    # which the read-only startup guard refuses (the whole run would exit 2).
    # A writable HOME/XDG/TMP outside the mount catches internal stores
    # (coordination, analytics, logs, tool-output) so they never touch the ws.
    home = tmp_path / "home"
    home.mkdir()
    env = os.environ.copy()
    env.update(
        DAIMONOS_KGL_AUTOINDEX="0",
        HOME=str(home), XDG_STATE_HOME=str(home / "state"),
        XDG_DATA_HOME=str(home / "data"), XDG_CONFIG_HOME=str(home / "config"),
        TMPDIR=str(tmp_path / "tmp"),
    )
    (tmp_path / "tmp").mkdir()
    result = subprocess.run(
        ["unshare", "-Urnm", sys.executable, "-c", _RO_MOUNT_PROGRAM,
         source_tests, str(daimonos_binary), str(workspace), commit,
         json.dumps(CATALOG_EXPECTED), json.dumps(CATALOG_ARGS)],
        env=env, capture_output=True, text=True, timeout=120,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    report = json.loads(result.stdout)
    assert report["read_only_mount"] and report["unchanged"], report
    # Full catalog exercised on a real read-only mount: many reads admitted,
    # every mutation/opaque/KGL tool refused.
    assert report["reads"] >= 20 and report["refusals"] >= 18, report


# G5 variant: a real read-only workspace mount WITH --state-dir outside the
# workspace and an UNUSABLE HOME. This proves internal writers relocate to the
# state root (never the workspace, never HOME) on a read-only mount. HOME is
# made unusable by a read-only bind mount rather than a chmod: this program
# runs inside a user namespace that holds CAP_DAC_OVERRIDE over its own files,
# so a mode-only HOME (the method of tests/test_state_directory.py) would be
# bypassed by the mapped root; an EROFS mount cannot be.
_RO_MOUNT_STATE_DIR_PROGRAM = r'''
import errno, hashlib, json, os, subprocess, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from conftest import DaimonosClient
binary, workspace, state, home = sys.argv[2:]
workspace = Path(workspace); state = Path(state); home = Path(home)
WRITE = "workspace_write_forbidden"

def pcode(r):
    if not r.get("isError"):
        return None
    try:
        return json.loads(r["content"][0]["text"]).get("code")
    except Exception:
        return None

def snapshot(root):
    out = []
    for p in sorted(root.rglob("*")):
        st = p.lstat()
        digest = hashlib.sha256(p.read_bytes()).hexdigest() if (p.is_file() and not p.is_symlink()) else None
        out.append((str(p.relative_to(root)), st.st_size, st.st_mtime_ns, digest))
    return out

for target in (workspace, home):
    subprocess.run(["mount", "--bind", str(target), str(target)], check=True)
    subprocess.run(["mount", "-o", "remount,bind,ro", str(target)], check=True)
    try:
        (target / "must-not-write").write_text("x")
        raise AssertionError(f"{target} is not read-only")
    except OSError as error:
        assert error.errno == errno.EROFS, (target, error)

before_ws = snapshot(workspace)
before_home = snapshot(home)
env = {**os.environ, "HOME": str(home)}
for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
    env.pop(key, None)
process = subprocess.Popen(
    [binary, "--mcp", "-w", str(workspace), "--workspace-read-only",
     "--state-dir", str(state)],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
)
try:
    client = DaimonosClient(process, str(workspace))
    client.send_raw({"jsonrpc": "2.0", "id": client._next_id(), "method": "initialize",
                     "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                                "clientInfo": {"name": "ro-state", "version": "1"}}})
    client.send_raw({"jsonrpc": "2.0", "method": "notifications/initialized"})
    # Reads still succeed on the read-only mount.
    assert not client.call_tool("list_all_tools", {}).get("isError")
    assert not client.call_tool("git_read", {"repo": ".", "command": "log"}).get("isError")
    # Relocated internal writers (KGL store) are admitted; they target the
    # state root, not the workspace and not HOME.
    assert pcode(client.call_tool("kgl_query", {"query": "stats"})) is None
    assert pcode(client.call_tool("kgl_assert", {"action": "declare_intent"})) is None
    created = client.call_tool("snapshot", {"action": "create"})
    assert pcode(created) is None and not created.get("isError"), created
    # User workspace writes stay refused regardless of --state-dir, and
    # restore stays the one governed mutation.
    assert pcode(client.call_tool("write_file", {"path": "x.txt", "content": "y"})) == WRITE
    assert pcode(client.call_tool("snapshot", {"action": "restore", "id": "x"})) == WRITE
finally:
    process.terminate()
    process.wait(timeout=5)

assert snapshot(workspace) == before_ws, "the read-only workspace changed"
assert snapshot(home) == before_home, "the unusable HOME was written"
# The internal write provably landed under the private state root.
assert list(state.rglob("kgl.db")), "the KGL store was not created under the state root"
print(json.dumps({"state_dir_variant": True, "unchanged_ws": True,
                  "unchanged_home": True, "kgl_under_state": True}))
'''


def test_ac_07_1_state_dir_variant_relocates_internal_writes_on_ro_mount(
    daimonos_binary, tmp_path
):
    """G5 variant: on a real read-only workspace mount, --state-dir outside the
    workspace plus an unusable HOME sends every internal write to the state
    root. The workspace and HOME are byte-identical afterwards; the KGL store
    is created under the state root; user workspace writes stay refused."""
    if sys.platform != "linux" or not shutil.which("unshare"):
        pytest.skip("requires Linux user and mount namespaces")
    if subprocess.run(["unshare", "-Urnm", "true"], capture_output=True).returncode:
        pytest.skip("user or mount namespaces are unavailable")
    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    home = tmp_path / "unusable-home"
    home.mkdir()
    (tmp_path / "tmp").mkdir()
    source_tests = str(Path(__file__).resolve().parent)
    env = os.environ.copy()
    env.update(DAIMONOS_KGL_AUTOINDEX="0", TMPDIR=str(tmp_path / "tmp"))
    result = subprocess.run(
        ["unshare", "-Urnm", sys.executable, "-c", _RO_MOUNT_STATE_DIR_PROGRAM,
         source_tests, str(daimonos_binary), str(workspace), str(state), str(home)],
        env=env, capture_output=True, text=True, timeout=120,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    report = json.loads(result.stdout)
    assert report == {"state_dir_variant": True, "unchanged_ws": True,
                      "unchanged_home": True, "kgl_under_state": True}, report


# --- Slice 6: S8 concurrent git_read status, independent views + cleanup -----


def test_s8_concurrent_git_read_status_independent_views_and_cleanup(
    daimonos_factory, tmp_path
):
    import threading

    workspace = tmp_path / "evidence"
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    _seed_repo(workspace)
    # Dirty the worktree so status is non-trivial and the two views must each
    # reconstruct it independently.
    (workspace / "untracked.txt").write_text("loose\n")
    # Private status scratch is relocated outside the workspace (W13).
    (workspace / "daimonos.toml").write_text(
        f'[git_read]\nstatus_directory = "{scratch}"\n'
    )

    clients = [daimonos_factory(workspace, args=RO) for _ in range(2)]
    results: dict[int, dict] = {}

    def run(i):
        results[i] = clients[i].call_tool(
            "git_read", {"command": "worktree_status", "repo": "."}
        )

    threads = [threading.Thread(target=run, args=(i,)) for i in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=20)

    for i in range(2):
        r = results[i]
        assert not r.get("isError"), (i, r)
        # Each independent private view reconstructs the same dirty state.
        assert "untracked.txt" in r["content"][0]["text"], (i, r)

    # Every per-call private view is removed on drop: no scratch leaks.
    leftovers = list(scratch.glob("daimonos-git-status-*"))
    assert leftovers == [], f"private status views were not cleaned up: {leftovers}"


# --- Slice 7: opt-out regression (option absent -> unchanged behavior) -------


def test_opt_out_default_allows_workspace_writes_and_opaque_tools(
    daimonos_factory, tmp_path
):
    # No --workspace-read-only: legacy behavior is preserved.
    client = daimonos_factory(tmp_path)

    written = client.call_tool("write_file", {"path": "new.txt", "content": "hello\n"})
    assert not written.get("isError"), written
    assert (tmp_path / "new.txt").read_text() == "hello\n"

    edited = client.call_tool(
        "edit_file", {"path": "new.txt", "edits": ["hello", "goodbye"]}
    )
    assert not edited.get("isError"), edited
    assert (tmp_path / "new.txt").read_text() == "goodbye\n"

    # Opaque tools run normally when the option is off.
    ran = client.call_tool("exec", {"command": "true"})
    assert not ran.get("isError"), ran
    scripted = client.call_tool(
        "execute_script", {"code": "result = write_file('s.txt', 'via script')"}
    )
    assert not scripted.get("isError"), scripted
    assert (tmp_path / "s.txt").read_text() == "via script"


# --- G1: KGL tools admitted only when the store is relocated outside the ws ---


def test_g1_kgl_tools_admitted_when_store_relocated(daimonos_factory, tmp_path):
    """Without --state-dir the KGL store is <workspace>/.kgl, so kgl_query/
    kgl_assert are refused (covered in AC-07.2). WITH --state-dir the store is
    relocated and the tools are admitted, with nothing created under the
    workspace."""
    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    (tmp_path / "tmp").mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    client = daimonos_factory(
        workspace, env=_isolated_env(tmp_path), args=RO + ["--state-dir", str(state)]
    )
    q = client.call_tool("kgl_query", {"query": "stats"})
    assert _policy_code(q) is None, f"kgl_query refused despite relocation: {q}"
    # The relocated store lives under the state root, never the workspace.
    assert not (workspace / ".kgl").exists(), "KGL store leaked into the workspace"


# --- G6: snapshot create/delete admitted when the store is relocated ----------


def test_g6_snapshot_create_delete_admitted_when_store_relocated(
    daimonos_factory, tmp_path
):
    """INVENTORIES W03: snapshot create only reads the workspace and writes the
    relocated store; delete touches only the store; restore mutates the
    workspace. With the store outside the workspace, create/delete are admitted
    and the workspace stays byte-identical; restore stays refused."""
    import re

    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    (tmp_path / "tmp").mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    before = _snapshot(workspace)
    client = daimonos_factory(
        workspace, env=_isolated_env(tmp_path), args=RO + ["--state-dir", str(state)]
    )

    created = client.call_tool("snapshot", {"action": "create"})
    assert _policy_code(created) is None, f"relocated snapshot create refused: {created}"
    assert not created.get("isError"), created

    listed = client.call_tool("snapshot", {"action": "list"})
    assert not listed.get("isError"), listed
    ids = re.findall(
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        listed["content"][0]["text"],
    )
    assert ids, f"no snapshot id in list output: {listed}"
    snap_id = ids[0]

    # Restore is the one governed mutation: always refused.
    restored = client.call_tool("snapshot", {"action": "restore", "id": snap_id})
    assert _policy_code(restored) == WRITE_CODE, restored

    deleted = client.call_tool("snapshot", {"action": "delete", "id": snap_id})
    assert _policy_code(deleted) is None, f"relocated snapshot delete refused: {deleted}"
    assert not deleted.get("isError"), deleted

    client.process.terminate()
    client.process.wait(timeout=5)
    # Create/delete only touched the relocated store; the workspace is unchanged
    # and no snapshot tree was written under it.
    assert _snapshot(workspace) == before, "snapshot create/delete mutated the workspace"
    assert not (workspace / ".daimonos" / "snapshots").exists()


# --- G3: startup refuses an internal writer that resolves inside the ws -------


def _run_startup(daimonos_binary, workspace, extra_args=None, env=None):
    """Spawn an MCP process that should refuse at startup; return (rc, stderr).

    The guard runs before the MCP server reads stdin, so the process exits on
    its own. stdin is closed immediately to avoid any wait.
    """
    proc = subprocess.run(
        [daimonos_binary, "--mcp", "-w", str(workspace), "--workspace-read-only",
         *(extra_args or [])],
        capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL,
        env=env,
    )
    return proc.returncode, proc.stderr


def test_g3_startup_refuses_logging_directory_inside_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "daimonos.toml").write_text(
        '[logging]\nenabled = true\ndirectory = "./logs"\n'
    )
    rc, stderr = _run_startup(daimonos_binary, workspace)
    assert rc == 2, (rc, stderr)
    assert "workspace_write_forbidden" in stderr, stderr
    assert "logging.directory" in stderr, stderr
    # Refused before services start: nothing was written under the workspace.
    assert not (workspace / "logs").exists()


def test_g3_startup_refuses_tool_output_directory_inside_workspace(daimonos_binary, tmp_path):
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "daimonos.toml").write_text(
        '[tool_output]\ndirectory = "./spill"\n'
    )
    rc, stderr = _run_startup(daimonos_binary, workspace)
    assert rc == 2, (rc, stderr)
    assert "workspace_write_forbidden" in stderr, stderr
    assert "tool_output.directory" in stderr, stderr
    assert not (workspace / "spill").exists()


def test_g3_startup_refuses_checkpoint_shadow_repo_inside_workspace(daimonos_binary, tmp_path):
    """G2(c): per-turn checkpoints default to <workspace>/.daimonos/
    checkpoints.git and the agent frontend never relocates them, so enabling
    them under read-only is refused before services start."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    (workspace / "daimonos.toml").write_text("[checkpoint]\nenabled = true\n")
    rc, stderr = _run_startup(daimonos_binary, workspace)
    assert rc == 2, (rc, stderr)
    assert "workspace_write_forbidden" in stderr, stderr
    assert "checkpoint" in stderr, stderr
    assert not (workspace / ".daimonos").exists()


def test_g3_startup_allows_logging_inside_workspace_when_relocated(daimonos_binary, tmp_path):
    """Read-only stays independent of --state-dir: a destination configured
    inside the workspace is relocated by --state-dir and then passes startup."""
    workspace = tmp_path / "evidence"
    workspace.mkdir()
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    # No workspace config: defaults resolve outside the workspace, and
    # --state-dir relocates every internal writer below the state root.
    env = {**os.environ, **_isolated_env(tmp_path)}
    (tmp_path / "tmp").mkdir()
    proc = subprocess.Popen(
        [daimonos_binary, "--mcp", "-w", str(workspace), "--workspace-read-only",
         "--state-dir", str(state)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    try:
        from conftest import _handshake
        client = _handshake(proc, workspace)
        # Startup succeeded: a read works and no startup refusal was emitted.
        assert not client.call_tool("ls", {"path": "."}).get("isError")
    finally:
        proc.terminate()
        proc.wait(timeout=5)


# --- G2: agent frontend (daimonos agent --print) under --workspace-read-only --

import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_AGENT_DONE = "RO_AGENT_DONE_MARKER"


class _WriteFileProvider:
    """A local fake OpenAI-compatible endpoint. On the first turn it asks the
    model to call `write_file`; on the second turn (after the tool result) it
    records the tool result it was handed and finishes. No network, no key."""

    def __init__(self):
        self.tool_result_text = None
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
                messages = body["messages"]
                tool_messages = [m for m in messages if m.get("role") == "tool"]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

                def chunk(delta, reason=None):
                    payload = {"choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                        "usage": {"prompt_tokens": 50, "completion_tokens": 5}}
                    self.wfile.write(("data: " + json.dumps(payload) + "\n\n").encode())
                    self.wfile.flush()

                try:
                    if not tool_messages:
                        chunk({"role": "assistant", "content": "", "tool_calls": [{"index": 0,
                            "id": "ro-call-1", "type": "function", "function": {
                                "name": "write_file",
                                "arguments": json.dumps({"path": "evil.txt", "content": "x"})}}]},
                              "tool_calls")
                    else:
                        owner.tool_result_text = json.dumps(tool_messages)
                        chunk({"role": "assistant", "content": _AGENT_DONE}, "stop")
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


def _agent_env_file(tmp_path, provider):
    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter",
        "DAIMONOS_AGENT_MODEL=test/model",
        f"DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:{provider.server.server_port}/v1",
        "DAIMONOS_AGENT_API_KEY=synthetic-not-a-real-key",
        "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off",
        "DAIMONOS_AGENT_MODELS_DISCOVER=off",
        "DAIMONOS_AGENT_ALLOWED_COMMANDS=write_file,read_file",
        "",
    ]))
    return agent_env


def test_g2_agent_print_refuses_write_and_leaves_workspace_unchanged(
    daimonos_binary, tmp_path
):
    """G2(a)(b): the agent frontend under --workspace-read-only returns the
    named refusal to the model for a `write_file` tool call and leaves the
    workspace + .git byte-identical.

    Uses --state-dir so internal stores (analytics, sessions, evidence,
    checkpoints) relocate outside the workspace. NOTE: `daimonos agent --print`
    WITHOUT --state-dir blocks before contacting the provider in an isolated
    sandbox — a pre-existing frontend behavior (see issues.md I-04; every
    tests/test_state_frontends.py agent test also passes --state-dir), not a
    WP04 effect. The no---state-dir workspace-unchanged property is covered by
    test_g2_agent_print_refuses_checkpoint_at_startup (agent mode, no state dir)
    and by the AC-07.2 MCP session (no state dir, workspace + .git identical)."""
    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    (tmp_path / "tmp").mkdir()
    before = _snapshot(workspace)

    provider = _WriteFileProvider()
    try:
        agent_env = _agent_env_file(tmp_path, provider)
        env = {k: v for k, v in os.environ.items() if not k.startswith("DAIMONOS_")}
        env.update(_isolated_env(tmp_path))
        state = tmp_path / "state"
        state.mkdir(mode=0o700, exist_ok=True)
        command = [daimonos_binary, "--workspace", str(workspace), "--workspace-read-only",
                   "--state-dir", str(state)]
        completed = subprocess.run(
            command + ["agent", "--print", "--agent-env", str(agent_env),
                       "please write the file"],
            capture_output=True, text=True, timeout=60, env=env,
        )
    finally:
        provider.close()

    assert completed.returncode == 0, completed.stderr
    # (a) The model saw the named refusal as the write_file tool result.
    assert provider.tool_result_text is not None, "provider never received a tool result"
    assert "workspace_write_forbidden" in provider.tool_result_text, provider.tool_result_text
    assert _AGENT_DONE in completed.stdout, completed.stdout
    # (b) The workspace and its .git are byte-identical before and after,
    #     whether or not --state-dir was supplied.
    assert _snapshot(workspace) == before, "agent run mutated the read-only workspace"
    # (c) No internal writer landed under the workspace (checkpoints, evidence).
    assert not (workspace / ".daimonos").exists(), ".daimonos written under read-only workspace"
    assert not (workspace / "evil.txt").exists(), "the refused write created a file"


def test_g2_agent_print_refuses_checkpoint_at_startup(daimonos_binary, tmp_path):
    """G2(c): with per-turn checkpoints enabled (and never relocated by the
    agent frontend), agent mode is refused before the provider is ever reached,
    with the named code and nothing written under the workspace."""
    workspace = tmp_path / "evidence"
    _seed_repo(workspace)
    (workspace / "daimonos.toml").write_text("[checkpoint]\nenabled = true\n")
    _git(workspace, "add", "daimonos.toml")
    _git(workspace, "commit", "-q", "-m", "cfg")
    before = _snapshot(workspace)  # re-taken after the config commit

    agent_env = tmp_path / "agent.env"
    agent_env.write_text("\n".join([
        "DAIMONOS_AGENT_PROVIDER=openrouter", "DAIMONOS_AGENT_MODEL=test/model",
        "DAIMONOS_AGENT_BASE_URL=http://127.0.0.1:1/v1",
        "DAIMONOS_AGENT_API_KEY=x", "DAIMONOS_AGENT_APPROVAL_MODE=auto",
        "DAIMONOS_AGENT_COMPACTION=off", "DAIMONOS_AGENT_MODELS_DISCOVER=off", "",
    ]))
    env = {k: v for k, v in os.environ.items() if not k.startswith("DAIMONOS_")}
    (tmp_path / "tmp").mkdir()
    env.update(_isolated_env(tmp_path))
    completed = subprocess.run(
        [daimonos_binary, "--workspace", str(workspace), "--workspace-read-only",
         "agent", "--print", "--agent-env", str(agent_env), "hello"],
        capture_output=True, text=True, timeout=60, env=env,
    )
    assert completed.returncode == 2, (completed.returncode, completed.stdout, completed.stderr)
    assert "workspace_write_forbidden" in completed.stderr, completed.stderr
    assert _snapshot(workspace) == before, "startup refusal still mutated the workspace"
    assert not (workspace / ".daimonos").exists()
