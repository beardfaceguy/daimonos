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


# --- R1 (r2): content search must not read THROUGH an in-root symlink-to-file.
#     A symlink whose target is a file inside the root is listed by the walk but
#     must never be opened and scanned; its target's content stays invisible. ---


def test_no_follow_content_search_does_not_read_through_file_link(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "secret.txt").write_text("needle FILE_WALK_SENTINEL\n")
    sub = workspace / "sub"
    sub.mkdir()
    # In-root symlink to a file that lives inside the root (resolves in-root).
    (sub / "alias.txt").symlink_to("../secret.txt")
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    # Scoped to sub/, the only route to the needle is through the file symlink.
    result = client.call_tool("search", {"pattern": "needle", "mode": "content", "path": "sub"})
    assert not result.get("isError"), result
    payload = json.loads(result["content"][0]["text"])
    assert "FILE_WALK_SENTINEL" not in json.dumps(payload)
    assert payload["matches"] == [], payload


# --- R2 (r2): diff_files must not follow a symlink input under no-follow. The
#     call is refused with symlink_refused and the target's content is absent. ---


def test_no_follow_diff_refuses_symlink_input(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "secret.txt").write_text("DIFF_TARGET_SENTINEL\n")
    (workspace / "alias.txt").symlink_to("secret.txt")
    (workspace / "other.txt").write_text("other\n")
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    refused = client.call_tool(
        "diff_files", {"path_a": "alias.txt", "path_b": "other.txt"}
    )
    assert refused.get("isError") is True, refused
    error = json.loads(refused["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    assert error["target"] == "secret.txt"
    assert "DIFF_TARGET_SENTINEL" not in json.dumps(refused)


# --- R3 (r2): FR-12 surfaces the literal target text of EVERY refused link
#     under no-follow, including a link that escapes the root (it is text stored
#     in the workspace, not resolved content). Combined --confine-paths
#     --no-follow-symlinks: the escaping link's literal target is pinned and no
#     resolved outside content appears. ---


def test_no_follow_escaping_link_surfaces_literal_target_no_content(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside.txt"
    outside.write_text("OUTSIDE_RESOLVED_SENTINEL\n")
    # Link stored inside the root whose literal target escapes the root.
    (workspace / "escape.txt").symlink_to("../outside.txt")
    client = daimonos_factory(
        workspace,
        args=[
            "--state-dir", str(_state(tmp_path)),
            "--confine-paths", "--no-follow-symlinks",
        ],
    )
    refused = client.call_tool("read_file", {"path": "escape.txt"})
    assert refused.get("isError") is True, refused
    error = json.loads(refused["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    # Literal, unresolved link text is surfaced even though it escapes the root.
    assert error["target"] == "../outside.txt"
    # No resolved outside content is ever returned.
    assert "OUTSIDE_RESOLVED_SENTINEL" not in json.dumps(refused)


# --- Consumer inventory (r2): a symlink named as an explicit walk/scan SCOPE
#     must not be followed. Previously ls / content-search / filename-search
#     resolved the scope through the link-following open and listed or scanned
#     the target. Under no-follow each now refuses with symlink_refused + target
#     and discloses no target listing or content. ---


def test_no_follow_ls_scope_symlink_dir_refused(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    realdir = workspace / "realdir"
    realdir.mkdir()
    (realdir / "deep.txt").write_text("LS_SCOPE_SENTINEL")
    (workspace / "linkdir").symlink_to("realdir", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    result = client.call_tool("ls", {"path": "linkdir", "depth": 3})
    assert result.get("isError") is True, result
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    assert error["target"] == "realdir"
    assert "LS_SCOPE_SENTINEL" not in json.dumps(result)
    assert "deep.txt" not in json.dumps(result)


def test_no_follow_content_search_scope_symlink_file_refused(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "secret.txt").write_text("needle CONTENT_SCOPE_SENTINEL\n")
    (workspace / "alias.txt").symlink_to("secret.txt")
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    result = client.call_tool("search", {"pattern": "needle", "mode": "content", "path": "alias.txt"})
    assert result.get("isError") is True, result
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    assert error["target"] == "secret.txt"
    assert "CONTENT_SCOPE_SENTINEL" not in json.dumps(result)


def test_no_follow_filename_search_scope_symlink_dir_refused(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    realdir = workspace / "realdir"
    realdir.mkdir()
    (realdir / "treasure.txt").write_text("x")
    (workspace / "linkdir").symlink_to("realdir", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    result = client.call_tool("search", {"pattern": "treasure", "mode": "files", "path": "linkdir"})
    assert result.get("isError") is True, result
    error = json.loads(result["content"][0]["text"])
    assert error["code"] == "symlink_refused"
    assert error["target"] == "realdir"
    assert "treasure" not in json.dumps(result)



# --- Consumer inventory (r2): the `stat` tool has NO confined adapter, so under
#     an active filesystem authority (confine and/or no-follow) the dispatch
#     refuses it outright (fail-closed) and it never resolves or reads through a
#     link. Pin that: a stat whose only route to the target is an in-root
#     intermediate directory link discloses no target metadata or content. ---


def test_no_follow_stat_through_link_is_fail_closed(daimonos_factory, tmp_path):
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    realdir = workspace / "realdir"
    realdir.mkdir()
    (realdir / "target.txt").write_text("STAT_TARGET_SENTINEL")
    # In-root intermediate directory link; the final component is a regular file.
    (workspace / "alias").symlink_to("realdir", target_is_directory=True)
    client = daimonos_factory(
        workspace,
        args=["--state-dir", str(_state(tmp_path)), "--no-follow-symlinks"],
    )
    result = client.call_tool("stat", {"path": "alias/target.txt"})
    # stat is refused outright under an active authority (no confined adapter);
    # it never follows the intermediate link to the target.
    assert result.get("isError") is True, result
    # No target metadata (file type/size) or content is disclosed through the link.
    assert '"type": "file"' not in json.dumps(result)
    assert "STAT_TARGET_SENTINEL" not in json.dumps(result)
