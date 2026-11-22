"""`.github/scripts/release-version.sh`: the version that a merge to master releases.

CI releases `v<version>` when no tag exists for the version in Cargo.toml. The
script prints that version. It fails when the four files that name the version
do not agree, so a release never ships a binary that reports another version.
"""
import json
import pathlib
import subprocess

REPO = pathlib.Path(__file__).resolve().parent.parent
SCRIPT = REPO / ".github" / "scripts" / "release-version.sh"


def make_repo(root, cargo="1.2.3", lock="1.2.3", server="1.2.3", changelog="1.2.3"):
    root.mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_text(
        f'[package]\nname = "daimonos"\nversion = "{cargo}"\n\n[dependencies]\nserde = {{ version = "1" }}\n')
    (root / "Cargo.lock").write_text(
        '[[package]]\nname = "cc"\nversion = "9.9.9"\n\n'
        f'[[package]]\nname = "daimonos"\nversion = "{lock}"\n\n'
        '[[package]]\nname = "daimonos-extra"\nversion = "8.8.8"\n')
    (root / "server.json").write_text(json.dumps({"name": "x", "version": server, "packages": []}))
    (root / "CHANGELOG.md").write_text(
        f"# Changelog\n\n## [Unreleased]\n\n## [{changelog}] - 2026-01-01\n\n- entry\n\n## [0.0.1] - 2025-01-01\n")
    return root


def run(root):
    return subprocess.run(["bash", str(SCRIPT), str(root)], capture_output=True, text=True, timeout=30)


def test_prints_the_version_when_the_files_agree(tmp_path):
    done = run(make_repo(tmp_path))
    assert (done.returncode, done.stdout) == (0, "1.2.3\n"), done.stderr


def test_accepts_a_prerelease_version(tmp_path):
    done = run(make_repo(tmp_path, cargo="1.2.3-rc.1", lock="1.2.3-rc.1", server="1.2.3-rc.1", changelog="1.2.3-rc.1"))
    assert (done.returncode, done.stdout) == (0, "1.2.3-rc.1\n"), done.stderr


def test_fails_and_names_the_file_that_differs(tmp_path):
    cases = {
        "Cargo.lock": dict(lock="1.2.2"),
        "server.json": dict(server="1.2.2"),
        "CHANGELOG.md": dict(changelog="1.2.2"),
        "Cargo.toml": dict(cargo="not a version", lock="not a version", server="not a version", changelog="not a version"),
    }
    for name, change in cases.items():
        done = run(make_repo(tmp_path / name.replace(".", "-"), **change))
        assert done.returncode != 0, name
        assert done.stdout == "", name
        assert name in done.stderr, (name, done.stderr)


def test_the_repository_itself_is_consistent():
    done = run(REPO)
    assert done.returncode == 0, done.stderr
    cargo_version = [line for line in (REPO / "Cargo.toml").read_text().splitlines() if line.startswith("version = ")][0]
    assert cargo_version == f'version = "{done.stdout.strip()}"'


# --- `.github/scripts/release-on-merge.sh`: claim the tag and start the release ---

CLAIM = REPO / ".github" / "scripts" / "release-on-merge.sh"

FAKE_GH = r'''#!/usr/bin/env bash
# A fake `gh` for the tests. It appends each call to $GH_LOG and answers from
# $GH_EXISTING_TAGS (space-separated) and $GH_CREATE / $GH_LOOKUP / $GH_DISPATCH.
echo "$*" >> "$GH_LOG"
case "$1 $2" in
"api repos/o/r/git/matching-refs/tags/"*)
    [ "${GH_LOOKUP:-ok}" = "fail" ] && { echo "HTTP 503" >&2; exit 1; }
    wanted="${2#repos/o/r/git/matching-refs/tags/}"
    count=0
    for tag in $GH_EXISTING_TAGS; do
        # The real endpoint matches by prefix. The script must count exact matches.
        case "$tag" in "$wanted"*) [ "$tag" = "$wanted" ] && count=$((count + 1)) ;; esac
    done
    echo "$count"
    ;;
"api repos/o/r/git/refs")
    case "${GH_CREATE:-ok}" in
    ok) echo '{"ref":"created"}' ;;
    exists) echo '{"message":"Reference already exists"}'; echo "gh: Reference already exists (HTTP 422)" >&2; exit 1 ;;
    *) echo "gh: Resource not accessible by integration (HTTP 403)" >&2; exit 1 ;;
    esac
    ;;
"workflow run")
    [ "${GH_DISPATCH:-ok}" = "ok" ] || { echo "HTTP 403" >&2; exit 1; }
    ;;
*) echo "unexpected gh call: $*" >&2; exit 97 ;;
esac
'''


def claim(tmp_path, **gh):
    root = make_repo(tmp_path / "repo")
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir(parents=True)
    (fake_bin / "gh").write_text(FAKE_GH)
    (fake_bin / "gh").chmod(0o755)
    log = tmp_path / "gh.log"
    log.write_text("")
    env = {"PATH": f"{fake_bin}:/usr/bin:/bin", "GITHUB_REPOSITORY": "o/r", "GITHUB_SHA": "abc123",
           "GH_LOG": str(log), "GH_EXISTING_TAGS": gh.pop("existing", ""), **gh}
    done = subprocess.run(["bash", str(CLAIM), str(root)], capture_output=True, text=True, timeout=30, env=env)
    return done, log.read_text().splitlines()


def test_a_new_version_creates_the_tag_and_starts_the_release(tmp_path):
    done, calls = claim(tmp_path, existing="v1.2.2 v1.2.30")
    assert done.returncode == 0, done.stderr
    assert calls[1] == "api repos/o/r/git/refs -f ref=refs/tags/v1.2.3 -f sha=abc123"
    assert calls[2] == "workflow run release.yml --repo o/r --ref v1.2.3"
    assert len(calls) == 3


def test_an_existing_tag_releases_nothing(tmp_path):
    done, calls = claim(tmp_path, existing="v1.2.3")
    assert done.returncode == 0, done.stderr
    assert len(calls) == 1 and "matching-refs" in calls[0], calls
    assert "releases nothing" in done.stdout


def test_a_lost_race_for_the_tag_releases_nothing(tmp_path):
    done, calls = claim(tmp_path, GH_CREATE="exists")
    assert done.returncode == 0, done.stderr
    assert not any(call.startswith("workflow run") for call in calls), calls


def test_api_failures_fail_the_job_and_start_no_release(tmp_path):
    for index, failure in enumerate([{"GH_LOOKUP": "fail"}, {"GH_CREATE": "denied"}]):
        done, calls = claim(tmp_path / str(index), **failure)
        assert done.returncode != 0, failure
        assert not any(call.startswith("workflow run") for call in calls), calls
    # A failed start of the release fails the job, and the message names the recovery command.
    done, _ = claim(tmp_path / "dispatch", GH_DISPATCH="fail")
    assert done.returncode != 0
    assert "gh workflow run release.yml --ref v1.2.3" in done.stderr


def test_inconsistent_version_files_start_no_release(tmp_path):
    root = make_repo(tmp_path / "repo", server="1.2.2")
    done = subprocess.run(["bash", str(CLAIM), str(root)], capture_output=True, text=True, timeout=30,
                          env={"PATH": "/usr/bin:/bin", "GITHUB_REPOSITORY": "o/r", "GITHUB_SHA": "abc123"})
    assert done.returncode != 0
    assert "server.json" in done.stderr
