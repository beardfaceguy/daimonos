"""Independent no-follow-symlinks policy (FR-12, AC-12.1-12.2, ADR-022 / WP14).

`--no-follow-symlinks` is an option independent of `--confine-paths`. When
enabled, tools report a symbolic link as a link together with its target text
and never read or walk through it. Both options can be combined. Default
behaviour (option absent) is unchanged.
"""

import json

import pytest


def _state(tmp_path):
    state = tmp_path / "state"
    state.mkdir(mode=0o700)
    return state


# --- AC-12.1: a link to a file inside the root is reported as a link and its
#     content is not returned. ---


@pytest.mark.parametrize("extra", [[], ["--confine-paths"]])
def test_no_follow_read_in_root_link_reports_target(daimonos_factory, tmp_path, extra):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("INSIDE_SENTINEL")
    (workspace / "link.txt").symlink_to("inside.txt")
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks", *extra],
    )
    refused = client.call_tool("read_file", {"path": "link.txt"})
    assert refused.get("isError") is True
    error = json.loads(refused["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    assert error["target"] == "inside.txt"
    # The link's own content (its target's bytes) is never returned.
    assert "INSIDE_SENTINEL" not in json.dumps(refused)


def test_no_follow_read_through_intermediate_dir_link_refused(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "real").mkdir()
    (workspace / "real" / "f.txt").write_text("DEEP_SENTINEL")
    (workspace / "alias").symlink_to("real", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    refused = client.call_tool("read_file", {"path": "alias/f.txt"})
    assert refused.get("isError") is True
    error = json.loads(refused["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    assert error["target"] == "real"
    assert "DEEP_SENTINEL" not in json.dumps(refused)


# --- AC-12.2: search and recursive listing do not traverse links. ---


def test_no_follow_ls_lists_link_without_descending(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    realdir = workspace / "realdir"
    realdir.mkdir()
    (realdir / "deep.txt").write_text("DEEP")
    (workspace / "linkdir").symlink_to("realdir", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    result = client.call_tool("ls", {"path": ".", "depth": 3})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    names = [entry["n"] for entry in payload["entries"]]
    # The real directory is traversed; the symlinked directory is not.
    assert "realdir/deep.txt" in names
    link_entry = next(entry for entry in payload["entries"] if entry["n"] == "linkdir")
    assert link_entry.get("l") is True
    assert not any(name.startswith("linkdir/") for name in names), names


def test_no_follow_content_search_does_not_traverse_link(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    elsewhere = workspace / "elsewhere"
    elsewhere.mkdir()
    (elsewhere / "secret.txt").write_text("needle WALK_SENTINEL\n")
    sub = workspace / "sub"
    sub.mkdir()
    (sub / "portal").symlink_to("../elsewhere", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    # Scoped to sub/, the only route to the sentinel is through the symlink.
    result = client.call_tool("search", {"pattern": "needle", "mode": "content", "path": "sub"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert "WALK_SENTINEL" not in json.dumps(payload)
    assert not any("portal" in match.get("f", "") for match in payload["matches"]), payload


def test_no_follow_filename_search_does_not_traverse_link(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    elsewhere = workspace / "elsewhere"
    elsewhere.mkdir()
    (elsewhere / "treasure.txt").write_text("x")
    sub = workspace / "sub"
    sub.mkdir()
    (sub / "portal").symlink_to("../elsewhere", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    result = client.call_tool("search", {"pattern": "treasure", "mode": "files", "path": "sub"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    names = [entry["file"] for entry in payload["results"]]
    assert not any("treasure" in name for name in names), names


# --- Independent-mode combinations and opt-out regressions. ---


def test_confine_only_still_follows_in_root_link(daimonos_factory, tmp_path):
    """Confinement alone (no-follow absent) keeps following in-root links."""
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("INSIDE")
    (workspace / "link.txt").symlink_to("inside.txt")
    client = daimonos_factory(
        workspace, args=["--state-dir", str(_state(tmp_path)), "--confine-paths"]
    )
    result = client.call_tool("read_file", {"path": "link.txt"})
    assert not result.get("isError"), result
    assert json.loads(result["content"][0]["text"])["content"] == "INSIDE"


def test_no_follow_absent_default_follows_link(daimonos_factory, tmp_path):
    """Opt-out regression: with the option absent, behaviour is unchanged."""
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "inside.txt").write_text("INSIDE")
    (workspace / "link.txt").symlink_to("inside.txt")
    client = daimonos_factory(workspace)
    result = client.call_tool("read_file", {"path": "link.txt"})
    assert not result.get("isError"), result
    assert json.loads(result["content"][0]["text"])["content"] == "INSIDE"



# --- Git metadata/link consumers (WP14). git_read reads git objects, never the
#     filesystem, so it is inherently no-follow: a stored tree symlink entry is a
#     120000-mode blob whose content is the literal target text; the tool never
#     resolves through it. Git metadata links (e.g. a `.git` gitdir pointer) are
#     git-internal symbolic refs, covered by the confinement git_read tests. ---


def _git(repo, *args):
    import subprocess

    return subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout.strip()


def test_git_read_reports_stored_symlink_as_link_not_resolved(daimonos):
    import os

    repo = os.path.join(daimonos.workspace, "source")
    os.mkdir(repo)
    _git(repo, "init", "-b", "main")
    _git(repo, "config", "user.name", "Reviewer Fixture")
    _git(repo, "config", "user.email", "review@example.test")
    _git(repo, "config", "commit.gpgsign", "false")
    with open(os.path.join(repo, "real.txt"), "w") as handle:
        handle.write("TARGET_CONTENT\n")
    os.symlink("real.txt", os.path.join(repo, "link.txt"))
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "commit with symlink")

    tree = json.loads(
        daimonos.call_tool("git_read", {"command": "list_tree", "repo": "source"})[
            "content"
        ][0]["text"]
    )
    modes = {entry["name"]: entry["mode"] for entry in tree["entries"]}
    # The stored symlink is a 120000 blob entry, reported as a link.
    assert modes["link.txt"] == "120000"

    doc = json.loads(
        daimonos.call_tool(
            "git_read", {"command": "show_file", "repo": "source", "path": "link.txt"}
        )["content"][0]["text"]
    )
    # Reading the link blob yields the literal target text, never the target's
    # content: git_read does not resolve through the stored link.
    assert doc["content"].strip() == "real.txt"
    assert "TARGET_CONTENT" not in json.dumps(doc)
