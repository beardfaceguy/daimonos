"""Pinned, read-only git access over MCP (ADR-017)."""

import json
import subprocess
from pathlib import Path


def git(repo, *args):
    return subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout.strip()


def repository(workspace):
    repo = Path(workspace) / "source"
    repo.mkdir()
    git(repo, "init", "-b", "main")
    git(repo, "config", "user.name", "Reviewer Fixture")
    git(repo, "config", "user.email", "review@example.test")
    git(repo, "config", "commit.gpgsign", "false")
    (repo / "file.txt").write_text("first line\nsecond line\n")
    git(repo, "add", ".")
    git(repo, "commit", "-m", "Initial source")
    return repo, git(repo, "rev-parse", "HEAD")


def test_git_read_resolves_nested_repository(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command": "resolve", "repo": "source", "rev": "HEAD"})
    assert not response.get("isError"), response
    resolved = json.loads(response["content"][0]["text"])
    assert resolved["commits"] == [commit]
    assert resolved["object_type"] == "commit"
    assert resolved["repo"].endswith("/source")


def test_git_read_show_file_is_pinned_and_paginated(daimonos):
    repo, commit = repository(daimonos.workspace)
    (repo / "file.txt").write_text("uncommitted content\n")
    response = daimonos.call_tool("git_read", {"command": "show_file", "repo": "source", "rev": commit, "path": "file.txt", "offset": 1, "limit": 1})
    assert not response.get("isError"), response
    document = json.loads(response["content"][0]["text"])
    assert document["content"] == "second line\n"
    assert document["commits"] == [commit]


def test_git_read_lists_tree_entries(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command":"list_tree", "repo":"source"})
    assert not response.get("isError"), response
    tree = json.loads(response["content"][0]["text"])
    assert tree["commits"] == [commit]
    assert tree["entries"] == [{"name":"file.txt", "type":"blob", "mode":"100644", "size":23, "sha":git(Path(daimonos.workspace)/"source", "rev-parse", "HEAD:file.txt")}]


def test_git_read_grep_returns_pinned_lines(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command":"grep", "repo":"source", "pattern":"second"})
    assert not response.get("isError"), response
    found = json.loads(response["content"][0]["text"])
    assert found["commits"] == [commit]
    assert found["matches"] == [{"path":"file.txt", "line_number":2, "line":"second line"}]


def test_git_read_log_has_full_metadata(daimonos):
    _, commit = repository(daimonos.workspace)
    response = daimonos.call_tool("git_read", {"command":"log", "repo":"source", "limit":1})
    assert not response.get("isError"), response
    history = json.loads(response["content"][0]["text"])
    assert history["entries"][0]["sha"] == commit
    assert history["entries"][0]["author"] == "Reviewer Fixture"
    assert history["entries"][0]["subject"] == "Initial source"
    assert history["entries"][0]["author_date"]
    assert history["entries"][0]["committer_date"]


def test_git_read_diff_compares_commits_not_worktree(daimonos):
    repo, first = repository(daimonos.workspace)
    (repo/"file.txt").write_text("replacement\n")
    git(repo,"add",".")
    git(repo,"commit","-m","Second source")
    second=git(repo,"rev-parse","HEAD")
    response=daimonos.call_tool("git_read",{"command":"diff","repo":"source","a":first,"b":second,"mode":"stat"})
    assert not response.get("isError"), response
    diff=json.loads(response["content"][0]["text"])
    assert diff["commits"] == [first,second]
    assert diff["files"] == [{"path":"file.txt","added":1,"deleted":2}]


def test_git_read_show_commit_includes_root_changes(daimonos):
    _, commit = repository(daimonos.workspace)
    response=daimonos.call_tool("git_read",{"command":"show_commit","repo":"source","rev":commit,"patch":True})
    assert not response.get("isError"), response
    change=json.loads(response["content"][0]["text"])
    assert change["commit"]["sha"] == commit
    assert change["files"] == [{"path":"file.txt","added":2,"deleted":0}]
    assert "+first line" in change["patch"]


def test_git_read_refs_include_annotated_tags(daimonos):
    repo, commit = repository(daimonos.workspace)
    git(repo,"tag","-a","review","-m","Review snapshot")
    response=daimonos.call_tool("git_read",{"command":"refs","repo":"source"})
    assert not response.get("isError"), response
    refs=json.loads(response["content"][0]["text"])
    assert {r["name"] for r in refs["refs"]} == {"refs/heads/main","refs/tags/review"}
    assert all(r["commit_sha"] == commit for r in refs["refs"])


def test_git_read_ancestry_returns_merge_base(daimonos):
    repo, first = repository(daimonos.workspace)
    git(repo,"commit","--allow-empty","-m","Next")
    second=git(repo,"rev-parse","HEAD")
    response=daimonos.call_tool("git_read",{"command":"ancestry","repo":"source","a":first,"b":second})
    assert not response.get("isError"), response
    ancestry=json.loads(response["content"][0]["text"])
    assert ancestry["commits"] == [first,second]
    assert ancestry["is_ancestor"] is True
    assert ancestry["merge_base"] == first


def test_git_read_status_reports_dirty_state(daimonos):
    repo, commit = repository(daimonos.workspace)
    (repo/"file.txt").write_text("dirty\n")
    response=daimonos.call_tool("git_read",{"command":"worktree_status","repo":"source"})
    assert not response.get("isError"), response
    status=json.loads(response["content"][0]["text"])
    assert status["commits"] == [commit]
    assert status["entries"] == [{"status":" M","path":"file.txt"}]


def test_git_read_blame_uses_committed_lines(daimonos):
    _, commit = repository(daimonos.workspace)
    response=daimonos.call_tool("git_read",{"command":"blame","repo":"source","path":"file.txt","start_line":2,"end_line":2})
    assert not response.get("isError"), response
    blame=json.loads(response["content"][0]["text"])
    assert blame["commits"] == [commit]
    assert blame["lines"][0]["line_number"] == 2
    assert blame["lines"][0]["sha"] == commit
    assert blame["lines"][0]["author"] == "Reviewer Fixture"
    assert blame["lines"][0]["line"] == "second line"
    assert blame["lines"][0]["date"]


def test_git_read_lists_schema_and_rejects_unknown_arguments(daimonos_factory, tmp_path):
    (tmp_path / "daimonos.toml").write_text("[mcp]\nfull_tool_schemas=true\n")
    daimonos = daimonos_factory(tmp_path)
    daimonos.call_tool("list_all_tools", {})
    tool = next(t for t in daimonos.list_tools() if t["name"] == "git_read")
    schema = tool["inputSchema"]
    assert schema["type"] == "object"
    assert schema["required"] == ["command"]
    # The MCP SDK drops root additionalProperties. The request parser still rejects extras.
    properties = schema["properties"]
    assert len(properties["command"]["enum"]) == 11
    assert properties["repo"]["type"] == "string"
    assert properties["paths"]["items"]["type"] == "string"
    assert properties["offset"]["type"] == "integer"
    response = daimonos.call_tool("git_read", {"command":"raw", "args":["status"]})
    assert response.get("isError")
    assert json.loads(response["content"][0]["text"])["error"]["kind"] == "rejected_argument"


def test_git_read_workspace_does_not_follow_session_cwd(daimonos):
    _, commit = repository(daimonos.workspace)
    nested = Path(daimonos.workspace) / "other"
    nested.mkdir()
    daimonos.call_tool("set_cwd", {"path":str(nested)})
    response = daimonos.call_tool("git_read", {"command":"resolve", "repo":"source"})
    assert not response.get("isError"), response
    assert json.loads(response["content"][0]["text"])["commits"] == [commit]


def test_policy_audit_mcp_delegates_ignore_agent_env(daimonos_factory, tmp_path):
    """Report-only reproduction. MCP does not enforce the agent policy."""
    client = daimonos_factory(tmp_path, env={
        "DAIMONOS_AGENT_ALLOWED_COMMANDS":"read_file,search,git_read,batch,execute_script",
        "DAIMONOS_AGENT_DENIED_COMMANDS":"write_file",
    })
    direct = client.call_tool("write_file", {"path":"direct.txt","content":"written"})
    batch = client.call_tool("batch", {"ops":[{
        "tool":"write_file", "arguments":{"path":"batch.txt","content":"written"}
    }]})
    script = client.call_tool("execute_script", {"code":'result = write_file("script.txt", "written")'})
    assert not direct.get("isError"), direct
    assert not batch.get("isError"), batch
    assert not script.get("isError"), script
    for name in ("direct.txt", "batch.txt", "script.txt"):
        assert (tmp_path / name).read_text() == "written"


def test_git_read_does_not_spill_large_results_to_disk(daimonos_factory, tmp_path):
    spill = tmp_path / "spill"
    (tmp_path / "daimonos.toml").write_text(
        f'[tool_output]\nmax_bytes=1024\ndirectory="{spill}"\n'
    )
    repo, _ = repository(tmp_path)
    (repo / "file.txt").write_text("x" * 4096 + "\n")
    git(repo,"add",".")
    git(repo,"commit","-m","Large content")
    client=daimonos_factory(tmp_path)
    response=client.call_tool("git_read", {"command":"show_file","repo":"source","path":"file.txt"})
    assert response.get("isError"), response
    failure=json.loads(response["content"][0]["text"])
    assert failure["error"]["kind"] == "output_limit"
    assert not spill.exists()


def test_git_read_bare_repository_has_history_but_no_worktree_status(daimonos):
    _, commit=repository(daimonos.workspace)
    git(Path(daimonos.workspace),"clone","--bare","source","bare")
    status=daimonos.call_tool("git_read",{"command":"worktree_status","repo":"bare"})
    assert status.get("isError"),status
    assert json.loads(status["content"][0]["text"])["error"]["kind"]=="bare_repository"
    resolved=daimonos.call_tool("git_read",{"command":"resolve","repo":"bare","rev":"HEAD"})
    assert not resolved.get("isError"),resolved
    assert json.loads(resolved["content"][0]["text"])["commits"]==[commit]


def test_git_read_rejects_nested_revision_ranges(daimonos):
    repository(daimonos.workspace)
    for command in ("log","diff"):
        response=daimonos.call_tool("git_read",{"command":command,"repo":"source","rev":"HEAD..HEAD..HEAD"})
        assert response.get("isError"),response
        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"


def test_git_read_grep_frames_matches_without_final_newlines(daimonos):
    repo,_=repository(daimonos.workspace)
    (repo/"a.txt").write_text("needle without newline")
    (repo/"b.txt").write_text("needle with newline\n")
    (repo/"c.txt").write_text("one\nneedle two\nneedle three\n")
    (repo/"binary.bin").write_bytes(b"\x00needle\x00")
    git(repo,"add",".")
    git(repo,"commit","-m","Grep framing fixture")
    response=daimonos.call_tool("git_read",{"command":"grep","repo":"source","pattern":"needle"})
    assert not response.get("isError"),response
    assert json.loads(response["content"][0]["text"])["matches"]==[
        {"path":"a.txt","line_number":1,"line":"needle without newline"},
        {"path":"b.txt","line_number":1,"line":"needle with newline"},
        {"path":"c.txt","line_number":2,"line":"needle two"},
        {"path":"c.txt","line_number":3,"line":"needle three"},
    ]


def test_git_read_blame_continuation_keeps_the_original_start_line(daimonos):
    repository(daimonos.workspace)
    params={"command":"blame","repo":"source","path":"file.txt","start_line":1,"end_line":2,"limit":1}
    first=daimonos.call_tool("git_read",params)
    assert not first.get("isError"),first
    first_page=json.loads(first["content"][0]["text"])
    second=daimonos.call_tool("git_read",{**params,"offset":first_page["next_offset"]})
    assert not second.get("isError"),second
    second_page=json.loads(second["content"][0]["text"])
    assert [first_page["lines"][0]["line_number"],second_page["lines"][0]["line_number"]]==[1,2]
    assert second_page["truncated"] is False


def test_git_read_grep_reports_successful_stderr(daimonos_factory,tmp_path):
    import os
    import shutil
    import sys

    repo,_=repository(tmp_path)
    real_git=shutil.which("git")
    assert real_git
    wrapper_dir=tmp_path/"wrapper"
    wrapper_dir.mkdir()
    wrapper=wrapper_dir/"git"
    wrapper.write_text(
        f"#!{sys.executable}\nimport subprocess,sys\n"
        f"code=subprocess.run([{real_git!r},*sys.argv[1:]]).returncode\n"
        "if 'grep' in sys.argv[1:]: print('warning: incomplete search',file=sys.stderr)\n"
        "sys.exit(code)\n"
    )
    wrapper.chmod(0o755)
    client=daimonos_factory(tmp_path,env={"PATH":str(wrapper_dir)+os.pathsep+os.environ["PATH"]})
    response=client.call_tool("git_read",{"command":"grep","repo":"source","pattern":"first"})
    assert response.get("isError"),response
    failure=json.loads(response["content"][0]["text"])
    assert failure["error"]["kind"]=="git_error"
    assert "incomplete search" in failure["error"]["message"]


def test_git_read_short_inheritance_prefixes_do_not_leak_git_controls(daimonos_factory,tmp_path):
    repository(tmp_path)
    for index,prefix in enumerate(("G","")):
        trace=tmp_path/f"git-trace-{index}"
        (tmp_path/"daimonos.toml").write_text(f'[process]\ninherit_env_prefixes=["{prefix}"]\n')
        client=daimonos_factory(tmp_path,env={"GIT_TRACE":str(trace)})
        trace.unlink(missing_ok=True)
        response=client.call_tool("git_read",{"command":"resolve","repo":"source"})
        assert not response.get("isError"),response
        assert not trace.exists(),f"git controls leaked through prefix {prefix!r}"


def test_git_read_strict_policy_keeps_default_resolution_and_adds_bodies(daimonos_factory,tmp_path):
    repo,first=repository(tmp_path)
    git(repo,"commit","--allow-empty","-m","Body fixture","-m","Stored body.")
    commit=git(repo,"rev-parse","HEAD")
    ordinary=daimonos_factory(tmp_path)
    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")
    (tmp_path/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n")
    strict=daimonos_factory(tmp_path)
    for revision in (None,"HEAD","main",commit[:10],"HEAD~1",f"{first}..{commit}"):
        args={"command":"resolve","repo":"source"}
        if revision is not None:
            args["rev"]=revision
        response=strict.call_tool("git_read",args)
        assert response.get("isError"),response
        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
    resolved=strict.call_tool("git_read",{"command":"resolve","repo":"source","rev":commit})
    assert not resolved.get("isError"),resolved
    assert json.loads(resolved["content"][0]["text"])["commits"]==[commit]
    history=strict.call_tool("git_read",{"command":"log","repo":"source","rev":commit,"include_body":True,"limit":1})
    assert not history.get("isError"),history
    assert json.loads(history["content"][0]["text"])["entries"][0]["body"]=="Stored body.\n"
    assert not ordinary.call_tool("git_read",{"command":"resolve","repo":"source","rev":"HEAD"}).get("isError")


def test_git_read_strict_operations_on_a_real_read_only_bind_mount(daimonos_binary,tmp_path):
    import errno
    import os
    import shutil
    import sys
    import pytest

    if sys.platform!="linux" or not shutil.which("unshare"):
        pytest.skip("requires Linux user and mount namespaces")
    supported=subprocess.run(["unshare","-Urnm","true"],capture_output=True,text=True)
    if supported.returncode:
        pytest.skip("user or mount namespaces are unavailable")
    workspace=tmp_path/"evidence"
    workspace.mkdir()
    _,commit=repository(workspace)
    (workspace/"daimonos.toml").write_text("[git_read]\nstrict_pinning=true\n[logging]\nenabled=false\n[analytics]\nenabled=false\n")
    source_tests=str(Path(__file__).resolve().parent)
    program=r'''
import errno,hashlib,json,os,subprocess,sys
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from conftest import DaimonosClient
binary,workspace,commit=sys.argv[2:]
workspace=Path(workspace)
def snapshot():
    return [(str(p.relative_to(workspace)),p.lstat().st_size,p.lstat().st_mtime_ns,hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else None) for p in sorted(workspace.rglob("*"))]
subprocess.run(["mount","--bind",str(workspace),str(workspace)],check=True)
subprocess.run(["mount","-o","remount,bind,ro",str(workspace)],check=True)
try:
    (workspace/"must-not-write").write_text("forbidden")
    raise AssertionError("the mount is not read-only")
except OSError as error:
    assert error.errno==errno.EROFS,error
before=snapshot()
process=subprocess.Popen([binary,"--mcp","-w",str(workspace)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
try:
    client=DaimonosClient(process,str(workspace))
    client.send_raw({"jsonrpc":"2.0","id":client._next_id(),"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"readonly-probe","version":"1"}}})
    client.send_raw({"jsonrpc":"2.0","method":"notifications/initialized"})
    calls=[{"command":"resolve"},{"command":"show_file","path":"file.txt"},{"command":"list_tree"},{"command":"grep","pattern":"first"},{"command":"log","include_body":True},{"command":"show_commit","include_body":True},{"command":"diff","a":commit,"b":commit},{"command":"blame","path":"file.txt"},{"command":"ancestry","a":commit,"b":commit}]
    for args in calls:
        if args["command"] not in ("diff","ancestry"):
            args["rev"]=commit
        response=client.call_tool("git_read",{"repo":"source",**args})
        assert not response.get("isError"),(args,response)
        assert commit in json.loads(response["content"][0]["text"])["commits"]
    for command in ("refs","worktree_status"):
        response=client.call_tool("git_read",{"repo":"source","command":command})
        assert response.get("isError"),response
        assert json.loads(response["content"][0]["text"])["error"]["kind"]=="rejected_argument"
finally:
    process.terminate()
    process.wait(timeout=5)
assert snapshot()==before,"the evidence changed"
print(json.dumps({"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}))
'''
    env=os.environ.copy()
    env["DAIMONOS_KGL_AUTOINDEX"]="0"
    result=subprocess.run(["unshare","-Urnm",sys.executable,"-c",program,source_tests,str(daimonos_binary),str(workspace),commit],env=env,capture_output=True,text=True,timeout=60)
    assert result.returncode==0,result.stdout+result.stderr
    assert json.loads(result.stdout)=={"read_only_mount":True,"pinned_operations":9,"refused_operations":2,"unchanged":True}


def test_git_read_filter_added_after_inspection_cannot_execute(daimonos_factory,tmp_path):
    import os
    import shutil
    import sys

    repo,_=repository(tmp_path)
    included=tmp_path/"late-filter.cfg"
    included.write_text("")
    marker=tmp_path/"late-filter-executed"
    malicious=tmp_path/"late-filter.sh"
    malicious.write_text(f"#!/bin/sh\nprintf executed > '{marker}'\n")
    malicious.chmod(0o755)
    git(repo,"config","include.path",str(included))
    (repo/".gitattributes").write_text("file.txt filter=late\n")
    git(repo,"add",".gitattributes")
    git(repo,"commit","-m","Late filter attribute")
    original=(repo/"file.txt").read_bytes()
    (repo/"file.txt").write_bytes(b"X"*len(original))
    real_git=shutil.which("git")
    assert real_git
    wrapper_dir=tmp_path/"git-wrapper"
    wrapper_dir.mkdir()
    wrapper=wrapper_dir/"git"
    payload=f'[filter "late"]\nprocess = {malicious}\nrequired = true\n'
    trigger=tmp_path/"status-boundary-reached"
    wrapper.write_text(
        f"#!{sys.executable}\nimport pathlib,subprocess,sys\n"
        f"if 'status' in sys.argv[1:]:\n    pathlib.Path({str(included)!r}).write_text({payload!r})\n    pathlib.Path({str(trigger)!r}).write_text('triggered')\n"
        f"sys.exit(subprocess.run([{real_git!r},*sys.argv[1:]]).returncode)\n"
    )
    wrapper.chmod(0o755)
    client=daimonos_factory(tmp_path,env={"PATH":str(wrapper_dir)+os.pathsep+os.environ["PATH"]})
    trigger.unlink(missing_ok=True)
    included.write_text(payload)
    baseline=subprocess.run([real_git,"--no-optional-locks","status","--porcelain=v1"],cwd=repo,capture_output=True,text=True)
    assert marker.exists(),baseline.stdout+baseline.stderr
    marker.unlink()
    included.write_text("")
    response=client.call_tool("git_read",{"command":"worktree_status","repo":"source"})
    assert trigger.exists(),"the process-boundary race did not run"
    assert included.read_text()==payload
    assert not marker.exists(),"a filter introduced after the inspection executed"
    assert not response.get("isError"),response


def test_frozen_status_cannot_inherit_global_or_system_filters(daimonos_factory,tmp_path):
    import os
    import shutil

    workspace=tmp_path/"evidence"
    workspace.mkdir()
    repo,_=repository(workspace)
    original=(repo/"file.txt").read_bytes()
    (repo/"system.txt").write_bytes(original)
    (repo/".gitattributes").write_text("file.txt filter=fromglobal required\nsystem.txt filter=fromsystem required\n")
    git(repo,"add",".")
    git(repo,"commit","-m","Inherited filter attributes")
    for name in ("file.txt","system.txt"):
        (repo/name).write_bytes(b"X"*len(original))
    global_config=tmp_path/"global.gitconfig"
    system_config=tmp_path/"system.gitconfig"
    markers=[]
    for driver,config in (("fromglobal",global_config),("fromsystem",system_config)):
        marker=tmp_path/f"{driver}-executed"
        script=tmp_path/f"{driver}.sh"
        script.write_text(f"#!/bin/sh\nprintf executed > '{marker}'\n")
        script.chmod(0o755)
        config.write_text(f'[FILTER "{driver}"]\nPrOcEsS = {script}\nReQuIrEd = true\n')
        markers.append(marker)
    inherited={"GIT_CONFIG_GLOBAL":str(global_config),"GIT_CONFIG_SYSTEM":str(system_config),"GIT_CONFIG_NOSYSTEM":"0"}
    real_git=shutil.which("git")
    assert real_git
    for name,marker in zip(("file.txt","system.txt"),markers):
        baseline=subprocess.run([real_git,"--no-optional-locks","status","--porcelain=v1","--",name],cwd=repo,env={**os.environ,**inherited},capture_output=True,text=True)
        assert marker.exists(),baseline.stdout+baseline.stderr
        marker.unlink()
    client=daimonos_factory(workspace,env=inherited)
    for marker in markers:
        marker.unlink(missing_ok=True)
    response=client.call_tool("git_read",{"command":"worktree_status","repo":"source"})
    assert not response.get("isError"),response
    assert not any(marker.exists() for marker in markers)
    assert json.loads(response["content"][0]["text"])["clean"] is False
