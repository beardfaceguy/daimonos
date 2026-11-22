# Agent Review Log
**Protocol:** review-protocol.md v1.3
<!-- review thread_id="release-on-merge-1622-90b8275" -->

<!-- event id="request" artifact path="1622-release-on-merge/artifacts/round-1-review-request.diff" sha256="67262c4be6d3fed8ee7dca26b998a508ad6b9efb8e82794ae495ca9904a1ea13" -->
## Review Request — Round 1
**Task:** 1622 — CI: release on a merge to master that changes the version (PR #286)
**Protocol:** review-protocol.md v1.3 — respond using the Review Response format.

### Proposed Solution
Review the diff origin/master (5f19e24)..90b8275 of PR #286. Goal, chosen by the maintainer: each merge to master that changes the version produces a release, with no bot commits on master. Before: a merge ran only ci.yml; release.yml (5 builds, binary test, GitHub Release, MCP Registry publish) started only on a pushed v* tag or a manual start. Rule: after a push to master, when all CI jobs passed, the new ci.yml job release-on-merge runs .github/scripts/release-on-merge.sh. The script reads the version (release-version.sh, which fails when Cargo.toml, Cargo.lock, server.json, and CHANGELOG.md do not name one version). If no tag v<version> exists, it creates the tag at GITHUB_SHA through the git refs API and starts release.yml with `gh workflow run release.yml --ref <tag>`. If the tag exists, it does nothing. Design choice: the job starts release.yml by workflow_dispatch on the tag ref and does not call it as a reusable workflow. Reasons: a tag created with GITHUB_TOKEN does not trigger the push-tag event; workflow_dispatch is the documented exception that GITHUB_TOKEN can trigger; a reusable-workflow call would put nested jobs with write permissions into ci.yml, which GitHub can reject at startup for fork pull requests; and with a dispatch on the tag ref, github.ref is refs/tags/v<version>, so release.yml needs no logic change (only a comment). The complete release.yml after this change follows, because the diff shows only its comment and the review needs its conditions:

name: Release

# Starts on a `v*` tag. ci.yml creates that tag after a merge to master that
# changes the version (.github/scripts/release-on-merge.sh) and then starts this
# workflow on the tag ref, because a tag that the workflow token creates does
# not trigger it. A maintainer can also push a tag, or start the workflow
# manually on a tag ref to run a failed release again.
on:
  push:
    tags: ["v*"]
  workflow_dispatch:
    inputs:
      tag:
        description: "Release tag (e.g. v0.1.0). Leave empty for a dev build."
        required: false

env:
  CARGO_TERM_COLOR: always

jobs:
  build:
    name: Build ${{ matrix.target }}
    runs-on: ${{ matrix.runner }}
    strategy:
      matrix:
        # Linux release binaries inherit the build image's glibc. Keep these on
        # 24.04 until a newer minimum is chosen on purpose.
        include:
          - target: x86_64-unknown-linux-gnu
            runner: ubuntu-24.04
            artifact: daimonos-x86_64-linux
          - target: aarch64-unknown-linux-gnu
            runner: ubuntu-24.04
            artifact: daimonos-aarch64-linux
          - target: x86_64-unknown-linux-musl
            runner: ubuntu-24.04
            artifact: daimonos-x86_64-linux-musl
          - target: aarch64-apple-darwin
            runner: macos-latest
            artifact: daimonos-aarch64-macos
          - target: x86_64-apple-darwin
            runner: macos-latest
            artifact: daimonos-x86_64-macos

    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1

      # Must match rust-toolchain.toml: cargo builds with the pinned toolchain,
      # so targets installed on any other toolchain are invisible to it.
      - uses: dtolnay/rust-toolchain@c0e9df88980754dd93e5833b8dcb1b304c1fe173 # 1.96.0
        with:
          targets: ${{ matrix.target }}

      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32 # v2
        with:
          key: ${{ matrix.target }}

      - name: Install cross-compilation tools
        if: runner.os == 'Linux'
        run: |
          sudo apt-get update
          case "${{ matrix.target }}" in
            aarch64-unknown-linux-gnu)
              sudo apt-get install -y gcc-aarch64-linux-gnu
              echo "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc" >> "$GITHUB_ENV"
              ;;
            x86_64-unknown-linux-musl)
              sudo apt-get install -y musl-tools
              ;;
          esac

      - name: Build release binary
        run: cargo build --release --target ${{ matrix.target }}

      - name: Package binary
        run: |
          mkdir -p dist
          cp target/${{ matrix.target }}/release/daimonos dist/daimonos
          chmod +x dist/daimonos
          cd dist
          tar czf ${{ matrix.artifact }}.tar.gz daimonos
          if command -v sha256sum &>/dev/null; then
            sha256sum ${{ matrix.artifact }}.tar.gz > ${{ matrix.artifact }}.tar.gz.sha256
          else
            shasum -a 256 ${{ matrix.artifact }}.tar.gz > ${{ matrix.artifact }}.tar.gz.sha256
          fi

      - uses: actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1 # v7.1.0
        with:
          node-version: '20'

      - name: Build MCPB bundle
        env:
          GH_REF: ${{ github.ref }}
          GH_INPUT_TAG: ${{ github.event.inputs.tag }}
        run: |
          npm install -g @anthropic-ai/mcpb

          # Determine version from tag or Cargo.toml
          if [[ "$GH_REF" == refs/tags/v* ]]; then
            VERSION="${GH_REF#refs/tags/v}"
          elif [[ -n "$GH_INPUT_TAG" ]]; then
            VERSION="$GH_INPUT_TAG"
            VERSION="${VERSION#v}"
          else
            VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
          fi

          # Determine platform for this target
          case "${{ matrix.target }}" in
            *-apple-darwin) PLATFORM="darwin" ;;
            *-linux-*)      PLATFORM="linux" ;;
          esac

          # Create per-platform bundle directory
          BUNDLE_DIR=$(mktemp -d)
          cp target/${{ matrix.target }}/release/daimonos "$BUNDLE_DIR/daimonos"
          chmod +x "$BUNDLE_DIR/daimonos"

          # Generate platform-specific manifest from template
          python3 -c "
          import json, sys
          with open('mcpb/manifest.json') as f:
              m = json.load(f)
          m['version'] = '$VERSION'
          m['compatibility']['platforms'] = ['$PLATFORM']
          with open('$BUNDLE_DIR/manifest.json', 'w') as f:
              json.dump(m, f, indent=2)
          "

          # Pack the bundle (signing is best-effort; mcpb verify has a known bug in v2.x)
          mcpb pack "$BUNDLE_DIR" "dist/${{ matrix.artifact }}.mcpb"
          mcpb sign "dist/${{ matrix.artifact }}.mcpb" --self-signed || true

          # Generate SHA-256 for the .mcpb
          cd dist
          if command -v sha256sum &>/dev/null; then
            sha256sum ${{ matrix.artifact }}.mcpb > ${{ matrix.artifact }}.mcpb.sha256
          else
            shasum -a 256 ${{ matrix.artifact }}.mcpb > ${{ matrix.artifact }}.mcpb.sha256
          fi

      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7.0.2
        with:
          name: ${{ matrix.artifact }}
          path: |
            dist/${{ matrix.artifact }}.tar.gz*
            dist/${{ matrix.artifact }}.mcpb*
          retention-days: 30

  test:
    name: Test release binary
    runs-on: ubuntu-24.04
    needs: [build]
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1

      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8.0.2
        with:
          name: daimonos-x86_64-linux
          path: dist/

      - name: Extract and verify binary
        run: |
          cd dist && sha256sum -c daimonos-x86_64-linux.tar.gz.sha256
          tar xzf daimonos-x86_64-linux.tar.gz
          chmod +x daimonos
          ./daimonos --help

      - name: MCP smoke test
        run: |
          WORKSPACE=$(mktemp -d)
          echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | \
            dist/daimonos --mcp -w "$WORKSPACE" 2>/dev/null | \
            python3 -c "import sys,json; r=json.loads(sys.stdin.readline()); assert r['result']['serverInfo']['name']=='daimonos', f'unexpected: {r}'; print('MCP init OK')"

  release:
    name: Create GitHub Release
    runs-on: ubuntu-24.04
    needs: [build, test]
    if: startsWith(github.ref, 'refs/tags/v') || github.event.inputs.tag != ''
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1

      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8.0.2
        with:
          path: artifacts/

      - name: Determine tag
        id: tag
        env:
          GH_REF: ${{ github.ref }}
          GH_INPUT_TAG: ${{ github.event.inputs.tag }}
        run: |
          if [[ "$GH_REF" == refs/tags/* ]]; then
            echo "tag=${GH_REF#refs/tags/}" >> "$GITHUB_OUTPUT"
          else
            echo "tag=$GH_INPUT_TAG" >> "$GITHUB_OUTPUT"
          fi

      - name: Collect release assets
        run: |
          mkdir -p release
          find artifacts/ -name '*.tar.gz' -exec cp {} release/ \;
          find artifacts/ -name '*.mcpb' -exec cp {} release/ \;
          find artifacts/ -name '*.sha256' -exec cp {} release/ \;
          ls -la release/

      - name: Create release
        uses: softprops/action-gh-release@efb35369e0ad2afab669f228072c1b0d510eae64 # v3.0.3
        with:
          tag_name: ${{ steps.tag.outputs.tag }}
          name: Daimonos ${{ steps.tag.outputs.tag }}
          draft: false
          prerelease: ${{ contains(steps.tag.outputs.tag, '-') }}
          generate_release_notes: true
          files: release/*

  publish-registry:
    name: Publish to MCP Registry
    runs-on: ubuntu-24.04
    needs: [release]
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1

      - name: Install mcp-publisher
        run: |
          curl -L https://github.com/modelcontextprotocol/registry/releases/latest/download/mcp-publisher_linux_amd64.tar.gz | tar xz
          chmod +x mcp-publisher

      - name: Download MCPB bundles and update server.json
        env:
          GH_TOKEN: ${{ github.token }}
          GH_REF_NAME: ${{ github.ref_name }}
        run: |
          TAG="$GH_REF_NAME"
          VERSION="${TAG#v}"

          mkdir -p mcpb-release
          gh release download "$TAG" --pattern '*.mcpb' --dir mcpb-release
          ls -la mcpb-release/

          python3 -c "
          import json, hashlib, os

          tag = '$TAG'
          version = '$VERSION'

          bundles = {
              'daimonos-x86_64-linux.mcpb': 0,
              'daimonos-aarch64-linux.mcpb': 1,
              'daimonos-aarch64-macos.mcpb': 2,
              'daimonos-x86_64-macos.mcpb': 3,
              'daimonos-x86_64-linux-musl.mcpb': 4,
          }

          with open('server.json') as f:
              s = json.load(f)

          s['version'] = version

          for filename, idx in bundles.items():
              path = os.path.join('mcpb-release', filename)
              # Fix URL: replace placeholder with actual tag
              url = s['packages'][idx]['identifier']
              s['packages'][idx]['identifier'] = url.replace('VERSION_PLACEHOLDER', tag)
              if os.path.exists(path):
                  h = hashlib.sha256(open(path, 'rb').read()).hexdigest()
                  s['packages'][idx]['fileSha256'] = h
                  print(f'{filename}: {h}')
              else:
                  print(f'WARNING: {filename} not found')

          with open('server.json', 'w') as f:
              json.dump(s, f, indent=2)
          "

          echo "--- Updated server.json ---"
          cat server.json

      - name: Publish to MCP Registry
        run: |
          ./mcp-publisher login github-oidc
          ./mcp-publisher publish server.json

Verification by the worker: actionlint 1.7.12 has no finding on both workflows (the unknown-label finding for ubuntu-26.04 is ignored; it exists on master). tests/test_release_version.py has 9 tests, with a fake gh for the release rule; they pass. Two mutations of the script each fail one test. Against the real repository: the matching-refs lookup returns 1 exact match for v0.1.6, 0 for v0.1.7, and 7 prefix matches with 0 exact for v0.1; a create request for the existing tag returns 'Reference already exists' with exit 1. CI on the PR was running at the time of this request.

### Relevant Code / Diff
diff --git a/.github/scripts/release-on-merge.sh b/.github/scripts/release-on-merge.sh
new file mode 100755
index 0000000..1356474
--- /dev/null
+++ b/.github/scripts/release-on-merge.sh
@@ -0,0 +1,51 @@
+#!/usr/bin/env bash
+# Release on merge (Vikunja #1622). Run by ci.yml after a push to master, when
+# all CI jobs passed.
+#
+# Rule: release `v<version>` when no tag exists for the version in Cargo.toml.
+# So a merge that changes the version releases, and a merge that keeps the
+# version releases nothing.
+#
+# Steps: create the tag at the tested commit, then start release.yml on the
+# tag ref. That is the same run as a manual release from a pushed tag. A tag
+# that the workflow token creates does not start release.yml by itself, so
+# this script starts it.
+#
+# Usage: release-on-merge.sh [REPOSITORY_ROOT]
+# Environment: GITHUB_REPOSITORY, GITHUB_SHA, and a token for `gh` with
+# `contents: write` and `actions: write`.
+set -euo pipefail
+
+root="${1:-.}"
+here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
+
+version=$("$here/release-version.sh" "$root")
+tag="v$version"
+
+# The endpoint matches by prefix (v0.1.7 also returns v0.1.70), so count the
+# exact matches. An API failure stops the script: it is not "no tag".
+existing=$(gh api "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/$tag" \
+    --jq "[.[] | select(.ref == \"refs/tags/$tag\")] | length")
+if [[ "$existing" != "0" ]]; then
+    echo "Tag $tag exists. This merge releases nothing."
+    exit 0
+fi
+
+# Creating the ref is the claim: of two runs, only one succeeds.
+if ! created=$(gh api "repos/$GITHUB_REPOSITORY/git/refs" \
+    -f "ref=refs/tags/$tag" -f "sha=$GITHUB_SHA" 2>&1); then
+    if grep -q "Reference already exists" <<< "$created"; then
+        echo "Another run created $tag. This merge releases nothing."
+        exit 0
+    fi
+    echo "release-on-merge: could not create $tag: $created" >&2
+    exit 1
+fi
+echo "Created $tag at $GITHUB_SHA."
+
+if ! gh workflow run release.yml --repo "$GITHUB_REPOSITORY" --ref "$tag"; then
+    echo "release-on-merge: $tag exists, but the release did not start." >&2
+    echo "Later merges do not try again. Start it with: gh workflow run release.yml --ref $tag" >&2
+    exit 1
+fi
+echo "Started release.yml for $tag."
diff --git a/.github/scripts/release-version.sh b/.github/scripts/release-version.sh
new file mode 100755
index 0000000..63986b3
--- /dev/null
+++ b/.github/scripts/release-version.sh
@@ -0,0 +1,36 @@
+#!/usr/bin/env bash
+# Print the version that a merge to master releases: the package version in
+# Cargo.toml. Fail when Cargo.lock, server.json, or CHANGELOG.md names another
+# version, so a release never ships a binary that reports a different version.
+#
+# Usage: release-version.sh [REPOSITORY_ROOT]
+set -euo pipefail
+
+root="${1:-.}"
+
+fail() {
+    echo "release-version: $1" >&2
+    exit 1
+}
+
+# The first `version = "..."` line is the one of [package].
+version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
+if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
+    fail "Cargo.toml: the package version '$version' is not MAJOR.MINOR.PATCH"
+fi
+
+lock=$(grep -A 1 '^name = "daimonos"$' "$root/Cargo.lock" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
+if [[ "$lock" != "$version" ]]; then
+    fail "Cargo.lock: the daimonos package has version '$lock', Cargo.toml has '$version'"
+fi
+
+server=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' "$root/server.json")
+if [[ "$server" != "$version" ]]; then
+    fail "server.json: version is '$server', Cargo.toml has '$version'"
+fi
+
+if ! grep -q -F "## [$version]" "$root/CHANGELOG.md"; then
+    fail "CHANGELOG.md: no '## [$version]' section"
+fi
+
+echo "$version"
diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml
index 47b77ca..342a289 100644
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -116,3 +116,21 @@ jobs:
           name: daimonos-musl
           path: target/x86_64-unknown-linux-musl/release/daimonos
           retention-days: 7
+
+  # Release on merge (Vikunja #1622). After a push to master, when all jobs
+  # above passed: if no tag exists for the version in Cargo.toml, create the tag
+  # at this commit and start release.yml on it. A merge that keeps the version
+  # releases nothing. The rule and its failure cases are in the script.
+  release-on-merge:
+    name: Release on merge
+    runs-on: ubuntu-26.04
+    needs: [fmt, check, test, mcp-integration, android, smoke-test, musl-build]
+    if: github.event_name == 'push' && github.ref == 'refs/heads/master'
+    permissions:
+      contents: write # create the tag
+      actions: write # start release.yml
+    steps:
+      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
+      - run: .github/scripts/release-on-merge.sh
+        env:
+          GH_TOKEN: ${{ github.token }}
diff --git a/.github/workflows/release.yml b/.github/workflows/release.yml
index 56b866c..51a6ca9 100644
--- a/.github/workflows/release.yml
+++ b/.github/workflows/release.yml
@@ -1,5 +1,10 @@
 name: Release
 
+# Starts on a `v*` tag. ci.yml creates that tag after a merge to master that
+# changes the version (.github/scripts/release-on-merge.sh) and then starts this
+# workflow on the tag ref, because a tag that the workflow token creates does
+# not trigger it. A maintainer can also push a tag, or start the workflow
+# manually on a tag ref to run a failed release again.
 on:
   push:
     tags: ["v*"]
diff --git a/AGENTS.md b/AGENTS.md
index c904952..4a5628a 100644
--- a/AGENTS.md
+++ b/AGENTS.md
@@ -157,6 +157,7 @@ daimonos/
 │                                  #   repowise + the graphify KGL substrate)
 ├── .github/
 │   ├── RELEASE_TEMPLATE.md
+│   ├── scripts/                   # release-version.sh, release-on-merge.sh (see Releases)
 │   └── workflows/                 # ci.yml, deploy.yml, distro.yml, release.yml
 ├── graphify-out/                  # Local KGL substrate, gitignored (ADR 012)
 ├── .repowise/                     # Local repowise index, gitignored
@@ -513,6 +514,28 @@ After changing the ACP engine, a live check means installing the binary and
 restarting the ACP client (Zed) — the client spawns `daimonos acp` itself, so a
 stale binary silently keeps serving the old behavior.
 
+## Releases
+
+CI releases on a merge to `master` that changes the version (Vikunja #1622).
+
+- **Rule.** After a push to `master`, when all CI jobs passed, the job
+  `Release on merge` reads the version in `Cargo.toml`. If no tag `v<version>`
+  exists, it creates the tag at that commit and starts `release.yml` on it. If
+  the tag exists, it does nothing.
+- **To release,** set the same new version in `Cargo.toml`, `Cargo.lock`
+  (package `daimonos`), and `server.json`, and add a `## [<version>]` section to
+  `CHANGELOG.md`. Do this in the PR that must ship, or in a separate release
+  PR. `.github/scripts/release-version.sh` checks that the four files agree,
+  and `tests/test_release_version.py` runs that check on each PR.
+- **A PR that keeps the version releases nothing.** Its changes ship with the
+  next version change. Put their changelog entries under `## [Unreleased]`.
+- **`release.yml`** builds five targets, tests the binary, creates the GitHub
+  Release, and publishes to the MCP Registry.
+- **If the release run fails,** the tag exists without a release, and later
+  merges do not try again. Run the failed jobs again, or start it with
+  `gh workflow run release.yml --ref v<version>`.
+- A version with a `-` (for example `0.2.0-rc.1`) becomes a prerelease.
+
 ## Vikunja project management
 
 Keep Vikunja accurate **as you work**, not after being asked. Work goes in
diff --git a/CHANGELOG.md b/CHANGELOG.md
index 77ef3a1..fbfc69c 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -7,6 +7,12 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ## [Unreleased]
 
+### Changed
+
+- **Releases start from CI.** A merge to `master` that changes the version in
+  `Cargo.toml` creates the tag and starts the Release workflow. A merge that
+  keeps the version releases nothing.
+
 ## [0.1.6] - 2026-11-22
 
 This release has the 13 PRs that merged after v0.1.5 (#252 to #263, and #284).
diff --git a/tests/test_release_version.py b/tests/test_release_version.py
new file mode 100644
index 0000000..259d422
--- /dev/null
+++ b/tests/test_release_version.py
@@ -0,0 +1,149 @@
+"""`.github/scripts/release-version.sh`: the version that a merge to master releases.
+
+CI releases `v<version>` when no tag exists for the version in Cargo.toml. The
+script prints that version. It fails when the four files that name the version
+do not agree, so a release never ships a binary that reports another version.
+"""
+import json
+import pathlib
+import subprocess
+
+REPO = pathlib.Path(__file__).resolve().parent.parent
+SCRIPT = REPO / ".github" / "scripts" / "release-version.sh"
+
+
+def make_repo(root, cargo="1.2.3", lock="1.2.3", server="1.2.3", changelog="1.2.3"):
+    root.mkdir(parents=True, exist_ok=True)
+    (root / "Cargo.toml").write_text(
+        f'[package]\nname = "daimonos"\nversion = "{cargo}"\n\n[dependencies]\nserde = {{ version = "1" }}\n')
+    (root / "Cargo.lock").write_text(
+        '[[package]]\nname = "cc"\nversion = "9.9.9"\n\n'
+        f'[[package]]\nname = "daimonos"\nversion = "{lock}"\n\n'
+        '[[package]]\nname = "daimonos-extra"\nversion = "8.8.8"\n')
+    (root / "server.json").write_text(json.dumps({"name": "x", "version": server, "packages": []}))
+    (root / "CHANGELOG.md").write_text(
+        f"# Changelog\n\n## [Unreleased]\n\n## [{changelog}] - 2026-01-01\n\n- entry\n\n## [0.0.1] - 2025-01-01\n")
+    return root
+
+
+def run(root):
+    return subprocess.run(["bash", str(SCRIPT), str(root)], capture_output=True, text=True, timeout=30)
+
+
+def test_prints_the_version_when_the_files_agree(tmp_path):
+    done = run(make_repo(tmp_path))
+    assert (done.returncode, done.stdout) == (0, "1.2.3\n"), done.stderr
+
+
+def test_accepts_a_prerelease_version(tmp_path):
+    done = run(make_repo(tmp_path, cargo="1.2.3-rc.1", lock="1.2.3-rc.1", server="1.2.3-rc.1", changelog="1.2.3-rc.1"))
+    assert (done.returncode, done.stdout) == (0, "1.2.3-rc.1\n"), done.stderr
+
+
+def test_fails_and_names_the_file_that_differs(tmp_path):
+    cases = {
+        "Cargo.lock": dict(lock="1.2.2"),
+        "server.json": dict(server="1.2.2"),
+        "CHANGELOG.md": dict(changelog="1.2.2"),
+        "Cargo.toml": dict(cargo="not a version", lock="not a version", server="not a version", changelog="not a version"),
+    }
+    for name, change in cases.items():
+        done = run(make_repo(tmp_path / name.replace(".", "-"), **change))
+        assert done.returncode != 0, name
+        assert done.stdout == "", name
+        assert name in done.stderr, (name, done.stderr)
+
+
+def test_the_repository_itself_is_consistent():
+    done = run(REPO)
+    assert done.returncode == 0, done.stderr
+    cargo_version = [line for line in (REPO / "Cargo.toml").read_text().splitlines() if line.startswith("version = ")][0]
+    assert cargo_version == f'version = "{done.stdout.strip()}"'
+
+
+# --- `.github/scripts/release-on-merge.sh`: claim the tag and start the release ---
+
+CLAIM = REPO / ".github" / "scripts" / "release-on-merge.sh"
+
+FAKE_GH = r'''#!/usr/bin/env bash
+# A fake `gh` for the tests. It appends each call to $GH_LOG and answers from
+# $GH_EXISTING_TAGS (space-separated) and $GH_CREATE / $GH_LOOKUP / $GH_DISPATCH.
+echo "$*" >> "$GH_LOG"
+case "$1 $2" in
+"api repos/o/r/git/matching-refs/tags/"*)
+    [ "${GH_LOOKUP:-ok}" = "fail" ] && { echo "HTTP 503" >&2; exit 1; }
+    wanted="${2#repos/o/r/git/matching-refs/tags/}"
+    count=0
+    for tag in $GH_EXISTING_TAGS; do
+        # The real endpoint matches by prefix. The script must count exact matches.
+        case "$tag" in "$wanted"*) [ "$tag" = "$wanted" ] && count=$((count + 1)) ;; esac
+    done
+    echo "$count"
+    ;;
+"api repos/o/r/git/refs")
+    case "${GH_CREATE:-ok}" in
+    ok) echo '{"ref":"created"}' ;;
+    exists) echo '{"message":"Reference already exists"}'; echo "gh: Reference already exists (HTTP 422)" >&2; exit 1 ;;
+    *) echo "gh: Resource not accessible by integration (HTTP 403)" >&2; exit 1 ;;
+    esac
+    ;;
+"workflow run")
+    [ "${GH_DISPATCH:-ok}" = "ok" ] || { echo "HTTP 403" >&2; exit 1; }
+    ;;
+*) echo "unexpected gh call: $*" >&2; exit 97 ;;
+esac
+'''
+
+
+def claim(tmp_path, **gh):
+    root = make_repo(tmp_path / "repo")
+    fake_bin = tmp_path / "bin"
+    fake_bin.mkdir(parents=True)
+    (fake_bin / "gh").write_text(FAKE_GH)
+    (fake_bin / "gh").chmod(0o755)
+    log = tmp_path / "gh.log"
+    log.write_text("")
+    env = {"PATH": f"{fake_bin}:/usr/bin:/bin", "GITHUB_REPOSITORY": "o/r", "GITHUB_SHA": "abc123",
+           "GH_LOG": str(log), "GH_EXISTING_TAGS": gh.pop("existing", ""), **gh}
+    done = subprocess.run(["bash", str(CLAIM), str(root)], capture_output=True, text=True, timeout=30, env=env)
+    return done, log.read_text().splitlines()
+
+
+def test_a_new_version_creates_the_tag_and_starts_the_release(tmp_path):
+    done, calls = claim(tmp_path, existing="v1.2.2 v1.2.30")
+    assert done.returncode == 0, done.stderr
+    assert calls[1] == "api repos/o/r/git/refs -f ref=refs/tags/v1.2.3 -f sha=abc123"
+    assert calls[2] == "workflow run release.yml --repo o/r --ref v1.2.3"
+    assert len(calls) == 3
+
+
+def test_an_existing_tag_releases_nothing(tmp_path):
+    done, calls = claim(tmp_path, existing="v1.2.3")
+    assert done.returncode == 0, done.stderr
+    assert len(calls) == 1 and "matching-refs" in calls[0], calls
+    assert "releases nothing" in done.stdout
+
+
+def test_a_lost_race_for_the_tag_releases_nothing(tmp_path):
+    done, calls = claim(tmp_path, GH_CREATE="exists")
+    assert done.returncode == 0, done.stderr
+    assert not any(call.startswith("workflow run") for call in calls), calls
+
+
+def test_api_failures_fail_the_job_and_start_no_release(tmp_path):
+    for index, failure in enumerate([{"GH_LOOKUP": "fail"}, {"GH_CREATE": "denied"}]):
+        done, calls = claim(tmp_path / str(index), **failure)
+        assert done.returncode != 0, failure
+        assert not any(call.startswith("workflow run") for call in calls), calls
+    # A failed start of the release fails the job, and the message names the recovery command.
+    done, _ = claim(tmp_path / "dispatch", GH_DISPATCH="fail")
+    assert done.returncode != 0
+    assert "gh workflow run release.yml --ref v1.2.3" in done.stderr
+
+
+def test_inconsistent_version_files_start_no_release(tmp_path):
+    root = make_repo(tmp_path / "repo", server="1.2.2")
+    done = subprocess.run(["bash", str(CLAIM), str(root)], capture_output=True, text=True, timeout=30,
+                          env={"PATH": "/usr/bin:/bin", "GITHUB_REPOSITORY": "o/r", "GITHUB_SHA": "abc123"})
+    assert done.returncode != 0
+    assert "server.json" in done.stderr

### Known Concerns
1. The job runs only for a push to master, so no real run of it exists before the merge. The merge of this PR exercises only the 'tag exists' path. The release path is tested with a fake gh only.
2. Not verified: GITHUB_TOKEN with contents: write can create a tag ref at a commit; GITHUB_TOKEN with actions: write can dispatch release.yml on a tag ref; the MCP Registry OIDC login (mcp-publisher login github-oidc) accepts a run whose event is workflow_dispatch on a tag ref.
3. If the dispatch fails, or the release run fails, the tag exists without a release and later merges do nothing. Recovery is manual (documented in AGENTS.md).
4. A maintainer can still push a tag by hand. If that happens while CI for the same commit runs, the CI job later finds the tag and does nothing. The reverse order (CI creates the tag, then a manual push of the same tag) is rejected by git.
5. release-version.sh reads the first `version = "..."` line of Cargo.toml as the package version, and reads the Cargo.lock entry with grep -A 1.
6. shellcheck did not run on the two scripts on the worker host. CI runs shellcheck only through the product's shellcheck tool tests, not on these scripts.
7. The job has no concurrency group. Two CI runs for two master commits with the same new version race on the tag creation; the script treats 'Reference already exists' as a lost race.

### Specific Questions for Reviewer
1. Can this change start a release that the maintainer did not intend, or start two release runs for one tag?
2. Can a pull request (including one from a fork) run the release job, get the write token, or change what the job runs on master?
3. Is the token scope minimal, and is there an injection path through the version string, the tag, or other values into the shell or the API calls?
4. Are the conditions in release.yml correct for a workflow_dispatch on a tag ref (the release and publish-registry jobs, the VERSION and TAG values)?
5. Is the failure behavior acceptable: which failures leave a tag without a release, and is each one visible?
6. Do the tests prove the rule, or does a test pass for a wrong reason?

<!-- event id="review-1" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 1
**Position:** PARTIAL

### Blocking Concerns
B1. publish-registry derives TAG from github.ref_name, which is only correct on a tag push. For the new primary path — workflow_dispatch with the tag passed as the `tag` input and the workflow started `--ref <tag>` — the script claims github.ref will be refs/tags/v<version>, but `gh workflow run --ref` accepts tag refs only if the workflow file exists at that tag (it does here) AND the dispatch sets github.ref to that tag ref; however the proposed solution dispatches with `--ref "$tag"` and NO `-f tag=...` input. That means in release.yml the `release` job gate `startsWith(github.ref, 'refs/tags/v') || github.event.inputs.tag != ''` relies entirely on github.ref being the tag ref, and github.event.inputs.tag is empty. Meanwhile the 'Determine tag' step and the MCPB VERSION derivation handle refs/tags/* correctly, but a manual re-run via `gh workflow run release.yml --ref v<version>` per AGENTS.md also leaves inputs.tag empty — if the dispatch is instead done on a branch ref with only the input set (the other documented manual path per the workflow_dispatch input description), publish-registry's TAG="$GH_REF_NAME" resolves to the branch name (e.g. 'master'), producing a wrong `gh release download master` and a wrong version in server.json. publish-registry must determine the tag the same way the release job does (tag ref OR inputs.tag), not from github.ref_name alone; as written, one of the two documented dispatch paths publishes with a wrong tag or fails late in the registry publish step.
B2. Unverified token capabilities are load-bearing and the merge of this PR cannot exercise the release path (worker's own known concern). Two assumptions are untested against GitHub: (a) GITHUB_TOKEN with actions: write can dispatch release.yml on a tag ref that was just created in the same run, and (b) mcp-publisher login github-oidc accepts a workflow_dispatch run on a tag ref. If (a) fails, the failure mode is the already-acknowledged 'tag without release', but worse, the very first real release after merging will hit this cold. Before approval, verify at least (a) empirically — e.g. in a scratch repo or by a one-off manual dispatch test with a token restricted to the same scopes — or add an automatic fallback/alert (e.g. the job fails loudly with the recovery command, which it does, plus a check that the dispatched run actually appeared via `gh run list` after a short wait, since `gh workflow run` returns success on 202 even if the run is never created, such as when the ref is invalid for dispatch).
B3. `gh workflow run` exits 0 on HTTP 202 regardless of whether a run is actually created; GitHub validates the ref/workflow asynchronously in some failure modes, and dispatch on a tag ref is exactly the under-tested case. The script treats a zero exit as 'Started release.yml for $tag', so a silently-dropped dispatch leaves a tag with no release and a green CI job — invisible failure, contradicting the stated goal that each failure leaving a tag without a release is visible. Add a post-dispatch confirmation (poll `gh run list --workflow=release.yml --ref <tag>` or the runs API for a short window) and fail the job if no run appears.

### Non-Blocking Suggestions
S1. Run shellcheck on release-on-merge.sh and release-version.sh in CI (a tiny lint step or adding the scripts to an existing lint job). The worker notes shellcheck never ran on these scripts; they contain quoting-sensitive constructs (jq filter built by string interpolation with \"$tag\") that shellcheck and a quick manual audit would both cover.
S2. The version string is validated by the regex in release-version.sh before being interpolated into the jq filter and API calls, which closes the obvious injection path — but the jq filter is still built by embedding $tag into a double-quoted jq program. Prefer `--jq` with `--arg`-style safety via `gh api ... --jq '[.[] | select(.ref == $t)] | length' ... ` is not directly available in gh; instead pass the exact ref comparison using jq's env (`env.WANT`) with WANT exported, so the filter contains no interpolated shell data. Defense in depth in case the regex is ever loosened (e.g. build metadata '+' versions, which the regex currently rejects — also worth documenting that '+' SemVer versions are unsupported).
S3. Add a concurrency group (e.g. concurrency: release-on-merge, cancel-in-progress: false) on the release-on-merge job. The tag-creation race is handled correctly as a claim, but serializing removes the race entirely and avoids two ci.yml runs both spending the lookup/claim calls, and avoids any theoretical double-dispatch if two different versions land in quick succession and interleave.
S4. release-version.sh reads the first `version = "..."` line of Cargo.toml, which breaks if a dependency table or any other key precedes [package].version (e.g. workspace tables, or `version = "1"` written on its own line in [dependencies]). Use `cargo metadata --no-deps --format-version 1` or restrict the sed to the [package] section to make this robust against innocent Cargo.toml reordering; the current test fixture happens to match the assumption rather than prove it.

**Verdict:** REVISE

<!-- event id="rebuttal-1" artifact path="1622-release-on-merge/artifacts/round-1-rebuttal.diff" sha256="3bc2a2b1fb9394f763f798544ae91a9e5ba37d9b1ada90185a2cfb26b7b270b2" -->
## Rebuttal — Round 1
**Position:** PARTIAL

### Responses to Blocking Concerns
Re B1: ACCEPT — Correct. publish-registry took TAG from github.ref_name. That is the tag on a tag ref, but it is the branch name for the documented manual start on a branch with the `tag` input, so `gh release download master` would fail and server.json would get a wrong version. This defect existed before this PR; this PR makes workflow_dispatch the main path, so it is fixed here. Fix: publish-registry now determines TAG the same way as the release job (the tag ref of the run, else the `tag` input). In addition, release-on-merge.sh now passes the tag as the input too (`gh workflow run release.yml --ref <tag> -f tag=<tag>`), so the release job gate and both tag derivations hold through either source and do not rest on github.ref alone. The recovery command in AGENTS.md and in the script's error message has the same form. The publish-registry job after the change:

  publish-registry:
    name: Publish to MCP Registry
    runs-on: ubuntu-24.04
    needs: [release]
    permissions:
      contents: read
      id-token: write
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1

      - name: Install mcp-publisher
        run: |
          curl -L https://github.com/modelcontextprotocol/registry/releases/latest/download/mcp-publisher_linux_amd64.tar.gz | tar xz
          chmod +x mcp-publisher

      - name: Download MCPB bundles and update server.json
        env:
          GH_TOKEN: ${{ github.token }}
          GH_REF: ${{ github.ref }}
          GH_INPUT_TAG: ${{ github.event.inputs.tag }}
        run: |
          # The same tag as the release job: the tag ref of the run, else the
          # `tag` input. `github.ref_name` is a branch name when the workflow
          # is started on a branch with the `tag` input.
          if [[ "$GH_REF" == refs/tags/* ]]; then
            TAG="${GH_REF#refs/tags/}"
          else
            TAG="$GH_INPUT_TAG"
          fi
          VERSION="${TAG#v}"

          mkdir -
Re B2: ACCEPT — Verified empirically for (a), with the real GITHUB_TOKEN of this repository. A temporary workflow on a throwaway branch (run 45046568848, permissions contents: write and actions: write, the same as the new job) ran the real release-on-merge.sh with two sed changes that the probe printed and counted: the tag name was probe-1622-<run id> in place of v<version>, and the `-f tag=` input was removed, so the started Release run was a dev build with no GitHub Release and no registry publish. Log of the probe: 'Created probe-1622-45046568848 at 73a42ac...', then the URL of Release run 45046582097, then 'Started release.yml for probe-1622-45046568848'. The runs API listed that run as event workflow_dispatch, head_branch probe-1622-45046568848, head_sha 73a42ac, status in_progress. The second call of the script printed 'Tag probe-1622-45046568848 exists. This merge releases nothing.' and the number of dispatch runs for the commit stayed 1. So: the workflow token can create a tag ref, it can dispatch release.yml on a tag that the same run just created, the run is found by the confirmation lookup, and the second call starts nothing. The probe cancelled the Release run; all 8 jobs show cancelled, the release list is unchanged, and the probe tag and branch are deleted. Two limits of the probe: it did not pass the `-f tag=` input (with it, the run would have created a real release for a probe tag), and it used a tag that does not start with v. Part (b), the MCP Registry OIDC login from a workflow_dispatch run, is NOT verified. A test needs a real publish of a new version, so it cannot run before the first real release. Its failure mode is visible and late, not silent: the Release run fails in its last job after the GitHub Release exists, and the registry step can be run again. The PR description and AGENTS.md state this open point. The requested alternative (confirm that the dispatched run exists) is implemented under B3.
Re B3: ACCEPT — Correct: a zero exit of `gh workflow run` shows only that GitHub accepted the request. Fix: after the dispatch, release-on-merge.sh polls the runs API (`actions/workflows/release.yml/runs?event=workflow_dispatch&head_sha=<sha>`, filtered to head_branch == tag) up to RELEASE_CONFIRM_ATTEMPTS times (default 12) with RELEASE_CONFIRM_INTERVAL seconds between (default 5). If no run exists, the job fails with 'no release run exists after N lookups' and prints the recovery command. Tests in tests/test_release_version.py with the fake gh: the run appears at the third lookup and the job succeeds after exactly 3 lookups; the run never appears and the job fails after 3 lookups with the recovery command on stderr; the happy path asserts the lookup call. The probe under B2 exercised the lookup against the real API: head_branch of a dispatch run on a tag ref is the tag name, which is what the filter compares.

### Revised Code / Diff
diff --git a/.github/scripts/release-on-merge.sh b/.github/scripts/release-on-merge.sh
new file mode 100755
index 0000000..342d618
--- /dev/null
+++ b/.github/scripts/release-on-merge.sh
@@ -0,0 +1,76 @@
+#!/usr/bin/env bash
+# Release on merge (Vikunja #1622). Run by ci.yml after a push to master, when
+# all CI jobs passed.
+#
+# Rule: release `v<version>` when no tag exists for the version in Cargo.toml.
+# So a merge that changes the version releases, and a merge that keeps the
+# version releases nothing.
+#
+# Steps: create the tag at the tested commit, then start release.yml on the
+# tag ref. That is the same run as a manual release from a pushed tag. A tag
+# that the workflow token creates does not start release.yml by itself, so
+# this script starts it.
+#
+# Usage: release-on-merge.sh [REPOSITORY_ROOT]
+# Environment: GITHUB_REPOSITORY, GITHUB_SHA, and a token for `gh` with
+# `contents: write` and `actions: write`. Optional: RELEASE_CONFIRM_ATTEMPTS
+# (default 12) and RELEASE_CONFIRM_INTERVAL in seconds (default 5).
+set -euo pipefail
+
+root="${1:-.}"
+here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
+
+version=$("$here/release-version.sh" "$root")
+tag="v$version"
+
+# The endpoint matches by prefix (v0.1.7 also returns v0.1.70), so count the
+# exact matches. An API failure stops the script: it is not "no tag". The jq
+# filters read the tag from the environment, so they contain no shell data.
+export WANT_TAG="$tag" WANT_REF="refs/tags/$tag"
+existing=$(gh api "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/$tag" \
+    --jq '[.[] | select(.ref == env.WANT_REF)] | length')
+if [[ "$existing" != "0" ]]; then
+    echo "Tag $tag exists. This merge releases nothing."
+    exit 0
+fi
+
+# Creating the ref is the claim: of two runs, only one succeeds.
+if ! created=$(gh api "repos/$GITHUB_REPOSITORY/git/refs" \
+    -f "ref=refs/tags/$tag" -f "sha=$GITHUB_SHA" 2>&1); then
+    if grep -q "Reference already exists" <<< "$created"; then
+        echo "Another run created $tag. This merge releases nothing."
+        exit 0
+    fi
+    echo "release-on-merge: could not create $tag: $created" >&2
+    exit 1
+fi
+echo "Created $tag at $GITHUB_SHA."
+
+not_started() {
+    echo "release-on-merge: $tag exists, but $1." >&2
+    echo "Later merges do not try again. Start it with: gh workflow run release.yml --ref $tag -f tag=$tag" >&2
+    exit 1
+}
+
+# The run starts on the tag ref and also gets the tag as its input, so
+# release.yml finds the tag in both of the places that it reads.
+gh workflow run release.yml --repo "$GITHUB_REPOSITORY" --ref "$tag" -f "tag=$tag" ||
+    not_started "the release did not start"
+
+# `gh workflow run` reports success when GitHub accepts the request. That does
+# not show that a run exists. Wait for the run, so that a dropped request fails
+# this job and is not a tag without a release behind a green check.
+attempts="${RELEASE_CONFIRM_ATTEMPTS:-12}"
+interval="${RELEASE_CONFIRM_INTERVAL:-5}"
+for ((attempt = 1; attempt <= attempts; attempt++)); do
+    runs=$(gh api "repos/$GITHUB_REPOSITORY/actions/workflows/release.yml/runs?event=workflow_dispatch&head_sha=$GITHUB_SHA&per_page=50" \
+        --jq '[.workflow_runs[] | select(.head_branch == env.WANT_TAG)] | length')
+    if [[ "$runs" != "0" ]]; then
+        echo "Started release.yml for $tag."
+        exit 0
+    fi
+    if ((attempt < attempts)); then
+        sleep "$interval"
+    fi
+done
+not_started "no release run exists after $attempts lookups"
diff --git a/.github/scripts/release-version.sh b/.github/scripts/release-version.sh
new file mode 100755
index 0000000..7d89ba9
--- /dev/null
+++ b/.github/scripts/release-version.sh
@@ -0,0 +1,41 @@
+#!/usr/bin/env bash
+# Print the version that a merge to master releases: the package version in
+# Cargo.toml. Fail when Cargo.lock, server.json, or CHANGELOG.md names another
+# version, so a release never ships a binary that reports a different version.
+#
+# Usage: release-version.sh [REPOSITORY_ROOT]
+set -euo pipefail
+
+root="${1:-.}"
+
+fail() {
+    echo "release-version: $1" >&2
+    exit 1
+}
+
+# The `version = "..."` line of the [package] table, not of another table.
+version=$(awk '
+    /^\[/ { in_package = ($0 == "[package]") }
+    in_package && /^version = ".*"$/ { sub(/^version = "/, ""); sub(/"$/, ""); print; exit }
+' "$root/Cargo.toml")
+# MAJOR.MINOR.PATCH with an optional prerelease part. Build metadata ("+...")
+# is not supported: the version becomes a tag name and a part of API paths.
+if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
+    fail "Cargo.toml: the package version '$version' is not MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-PRERELEASE"
+fi
+
+lock=$(grep -A 1 '^name = "daimonos"$' "$root/Cargo.lock" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
+if [[ "$lock" != "$version" ]]; then
+    fail "Cargo.lock: the daimonos package has version '$lock', Cargo.toml has '$version'"
+fi
+
+server=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' "$root/server.json")
+if [[ "$server" != "$version" ]]; then
+    fail "server.json: version is '$server', Cargo.toml has '$version'"
+fi
+
+if ! grep -q -F "## [$version]" "$root/CHANGELOG.md"; then
+    fail "CHANGELOG.md: no '## [$version]' section"
+fi
+
+echo "$version"
diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml
index 47b77ca..2362ffd 100644
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -46,6 +46,8 @@ jobs:
       - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32 # v2
       - name: Install CLI test prerequisites
         run: sudo apt-get update && sudo apt-get install --yes npm shellcheck
+      - name: Lint the release scripts
+        run: shellcheck .github/scripts/*.sh
       - run: cargo test
 
   mcp-integration:
@@ -116,3 +118,21 @@ jobs:
           name: daimonos-musl
           path: target/x86_64-unknown-linux-musl/release/daimonos
           retention-days: 7
+
+  # Release on merge (Vikunja #1622). After a push to master, when all jobs
+  # above passed: if no tag exists for the version in Cargo.toml, create the tag
+  # at this commit and start release.yml on it. A merge that keeps the version
+  # releases nothing. The rule and its failure cases are in the script.
+  release-on-merge:
+    name: Release on merge
+    runs-on: ubuntu-26.04
+    needs: [fmt, check, test, mcp-integration, android, smoke-test, musl-build]
+    if: github.event_name == 'push' && github.ref == 'refs/heads/master'
+    permissions:
+      contents: write # create the tag
+      actions: write # start release.yml
+    steps:
+      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
+      - run: .github/scripts/release-on-merge.sh
+        env:
+          GH_TOKEN: ${{ github.token }}
diff --git a/.github/workflows/release.yml b/.github/workflows/release.yml
index 56b866c..ac95975 100644
--- a/.github/workflows/release.yml
+++ b/.github/workflows/release.yml
@@ -1,5 +1,12 @@
 name: Release
 
+# Starts on a `v*` tag. ci.yml creates that tag after a merge to master that
+# changes the version (.github/scripts/release-on-merge.sh) and then starts this
+# workflow on the tag ref, because a tag that the workflow token creates does
+# not trigger it. A maintainer can also push a tag, or start the workflow
+# manually on a tag ref to run a failed release again. A manual start on a
+# branch with the `tag` input creates that tag at the head of the default
+# branch. Without the input it is a dev build with no release.
 on:
   push:
     tags: ["v*"]
@@ -231,9 +238,17 @@ jobs:
       - name: Download MCPB bundles and update server.json
         env:
           GH_TOKEN: ${{ github.token }}
-          GH_REF_NAME: ${{ github.ref_name }}
+          GH_REF: ${{ github.ref }}
+          GH_INPUT_TAG: ${{ github.event.inputs.tag }}
         run: |
-          TAG="$GH_REF_NAME"
+          # The same tag as the release job: the tag ref of the run, else the
+          # `tag` input. `github.ref_name` is a branch name when the workflow
+          # is started on a branch with the `tag` input.
+          if [[ "$GH_REF" == refs/tags/* ]]; then
+            TAG="${GH_REF#refs/tags/}"
+          else
+            TAG="$GH_INPUT_TAG"
+          fi
           VERSION="${TAG#v}"
 
           mkdir -p mcpb-release
diff --git a/AGENTS.md b/AGENTS.md
index c904952..641c58a 100644
--- a/AGENTS.md
+++ b/AGENTS.md
@@ -157,6 +157,7 @@ daimonos/
 │                                  #   repowise + the graphify KGL substrate)
 ├── .github/
 │   ├── RELEASE_TEMPLATE.md
+│   ├── scripts/                   # release-version.sh, release-on-merge.sh (see Releases)
 │   └── workflows/                 # ci.yml, deploy.yml, distro.yml, release.yml
 ├── graphify-out/                  # Local KGL substrate, gitignored (ADR 012)
 ├── .repowise/                     # Local repowise index, gitignored
@@ -513,6 +514,30 @@ After changing the ACP engine, a live check means installing the binary and
 restarting the ACP client (Zed) — the client spawns `daimonos acp` itself, so a
 stale binary silently keeps serving the old behavior.
 
+## Releases
+
+CI releases on a merge to `master` that changes the version (Vikunja #1622).
+
+- **Rule.** After a push to `master`, when all CI jobs passed, the job
+  `Release on merge` reads the version in `Cargo.toml`. If no tag `v<version>`
+  exists, it creates the tag at that commit and starts `release.yml` on it. If
+  the tag exists, it does nothing.
+- **To release,** set the same new version in `Cargo.toml`, `Cargo.lock`
+  (package `daimonos`), and `server.json`, and add a `## [<version>]` section to
+  `CHANGELOG.md`. Do this in the PR that must ship, or in a separate release
+  PR. `.github/scripts/release-version.sh` checks that the four files agree,
+  and `tests/test_release_version.py` runs that check on each PR.
+- **A PR that keeps the version releases nothing.** Its changes ship with the
+  next version change. Put their changelog entries under `## [Unreleased]`.
+- **`release.yml`** builds five targets, tests the binary, creates the GitHub
+  Release, and publishes to the MCP Registry.
+- **If the release run fails,** the tag exists without a release, and later
+  merges do not try again. The job `Release on merge` fails when the release
+  run does not start. Run the failed jobs of the release run again, or start a
+  new run with `gh workflow run release.yml --ref v<version> -f tag=v<version>`.
+- A version with build metadata (`+...`) is not supported.
+- A version with a `-` (for example `0.2.0-rc.1`) becomes a prerelease.
+
 ## Vikunja project management
 
 Keep Vikunja accurate **as you work**, not after being asked. Work goes in
diff --git a/CHANGELOG.md b/CHANGELOG.md
index 77ef3a1..fbfc69c 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -7,6 +7,12 @@ and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.ht
 
 ## [Unreleased]
 
+### Changed
+
+- **Releases start from CI.** A merge to `master` that changes the version in
+  `Cargo.toml` creates the tag and starts the Release workflow. A merge that
+  keeps the version releases nothing.
+
 ## [0.1.6] - 2026-11-22
 
 This release has the 13 PRs that merged after v0.1.5 (#252 to #263, and #284).
diff --git a/tests/test_release_version.py b/tests/test_release_version.py
new file mode 100644
index 0000000..299c0b1
--- /dev/null
+++ b/tests/test_release_version.py
@@ -0,0 +1,185 @@
+"""`.github/scripts/release-version.sh`: the version that a merge to master releases.
+
+CI releases `v<version>` when no tag exists for the version in Cargo.toml. The
+script prints that version. It fails when the four files that name the version
+do not agree, so a release never ships a binary that reports another version.
+"""
+import json
+import pathlib
+import subprocess
+
+REPO = pathlib.Path(__file__).resolve().parent.parent
+SCRIPT = REPO / ".github" / "scripts" / "release-version.sh"
+
+
+def make_repo(root, cargo="1.2.3", lock="1.2.3", server="1.2.3", changelog="1.2.3"):
+    root.mkdir(parents=True, exist_ok=True)
+    (root / "Cargo.toml").write_text(
+        f'[package]\nname = "daimonos"\nversion = "{cargo}"\n\n[dependencies]\nserde = {{ version = "1" }}\n')
+    (root / "Cargo.lock").write_text(
+        '[[package]]\nname = "cc"\nversion = "9.9.9"\n\n'
+        f'[[package]]\nname = "daimonos"\nversion = "{lock}"\n\n'
+        '[[package]]\nname = "daimonos-extra"\nversion = "8.8.8"\n')
+    (root / "server.json").write_text(json.dumps({"name": "x", "version": server, "packages": []}))
+    (root / "CHANGELOG.md").write_text(
+        f"# Changelog\n\n## [Unreleased]\n\n## [{changelog}] - 2026-01-01\n\n- entry\n\n## [0.0.1] - 2025-01-01\n")
+    return root
+
+
+def run(root):
+    return subprocess.run(["bash", str(SCRIPT), str(root)], capture_output=True, text=True, timeout=30)
+
+
+def test_prints_the_version_when_the_files_agree(tmp_path):
+    done = run(make_repo(tmp_path))
+    assert (done.returncode, done.stdout) == (0, "1.2.3\n"), done.stderr
+
+
+def test_reads_the_version_of_the_package_table_only(tmp_path):
+    root = make_repo(tmp_path)
+    cargo = (root / "Cargo.toml").read_text()
+    (root / "Cargo.toml").write_text('[workspace.package]\nversion = "9.9.9"\n\n' + cargo)
+    done = run(root)
+    assert (done.returncode, done.stdout) == (0, "1.2.3\n"), done.stderr
+
+
+def test_rejects_a_version_with_build_metadata(tmp_path):
+    done = run(make_repo(tmp_path, cargo="1.2.3+b1", lock="1.2.3+b1", server="1.2.3+b1", changelog="1.2.3+b1"))
+    assert done.returncode != 0
+    assert "Cargo.toml" in done.stderr
+
+
+def test_accepts_a_prerelease_version(tmp_path):
+    done = run(make_repo(tmp_path, cargo="1.2.3-rc.1", lock="1.2.3-rc.1", server="1.2.3-rc.1", changelog="1.2.3-rc.1"))
+    assert (done.returncode, done.stdout) == (0, "1.2.3-rc.1\n"), done.stderr
+
+
+def test_fails_and_names_the_file_that_differs(tmp_path):
+    cases = {
+        "Cargo.lock": dict(lock="1.2.2"),
+        "server.json": dict(server="1.2.2"),
+        "CHANGELOG.md": dict(changelog="1.2.2"),
+        "Cargo.toml": dict(cargo="not a version", lock="not a version", server="not a version", changelog="not a version"),
+    }
+    for name, change in cases.items():
+        done = run(make_repo(tmp_path / name.replace(".", "-"), **change))
+        assert done.returncode != 0, name
+        assert done.stdout == "", name
+        assert name in done.stderr, (name, done.stderr)
+
+
+def test_the_repository_itself_is_consistent():
+    done = run(REPO)
+    assert done.returncode == 0, done.stderr
+    cargo_version = [line for line in (REPO / "Cargo.toml").read_text().splitlines() if line.startswith("version = ")][0]
+    assert cargo_version == f'version = "{done.stdout.strip()}"'
+
+
+# --- `.github/scripts/release-on-merge.sh`: claim the tag and start the release ---
+
+CLAIM = REPO / ".github" / "scripts" / "release-on-merge.sh"
+
+FAKE_GH = r'''#!/usr/bin/env bash
+# A fake `gh` for the tests. It appends each call to $GH_LOG and answers from
+# $GH_EXISTING_TAGS (space-separated) and $GH_CREATE / $GH_LOOKUP / $GH_DISPATCH.
+echo "$*" >> "$GH_LOG"
+case "$1 $2" in
+"api repos/o/r/git/matching-refs/tags/"*)
+    [ "${GH_LOOKUP:-ok}" = "fail" ] && { echo "HTTP 503" >&2; exit 1; }
+    wanted="${2#repos/o/r/git/matching-refs/tags/}"
+    count=0
+    for tag in $GH_EXISTING_TAGS; do
+        # The real endpoint matches by prefix. The script must count exact matches.
+        case "$tag" in "$wanted"*) [ "$tag" = "$wanted" ] && count=$((count + 1)) ;; esac
+    done
+    echo "$count"
+    ;;
+"api repos/o/r/git/refs")
+    case "${GH_CREATE:-ok}" in
+    ok) echo '{"ref":"created"}' ;;
+    exists) echo '{"message":"Reference already exists"}'; echo "gh: Reference already exists (HTTP 422)" >&2; exit 1 ;;
+    *) echo "gh: Resource not accessible by integration (HTTP 403)" >&2; exit 1 ;;
+    esac
+    ;;
+"workflow run")
+    [ "${GH_DISPATCH:-ok}" = "ok" ] || { echo "HTTP 403" >&2; exit 1; }
+    ;;
+"api repos/o/r/actions/workflows/release.yml/runs?"*)
+    # The run appears at lookup number $GH_RUN_AT (0 = never).
+    lookups=$(grep -c "actions/workflows/release.yml/runs" "$GH_LOG")
+    if [ "${GH_RUN_AT:-1}" != "0" ] && [ "$lookups" -ge "${GH_RUN_AT:-1}" ]; then echo 1; else echo 0; fi
+    ;;
+*) echo "unexpected gh call: $*" >&2; exit 97 ;;
+esac
+'''
+
+
+def claim(tmp_path, **gh):
+    root = make_repo(tmp_path / "repo")
+    fake_bin = tmp_path / "bin"
+    fake_bin.mkdir(parents=True)
+    (fake_bin / "gh").write_text(FAKE_GH)
+    (fake_bin / "gh").chmod(0o755)
+    log = tmp_path / "gh.log"
+    log.write_text("")
+    env = {"PATH": f"{fake_bin}:/usr/bin:/bin", "GITHUB_REPOSITORY": "o/r", "GITHUB_SHA": "abc123",
+           "RELEASE_CONFIRM_ATTEMPTS": "3", "RELEASE_CONFIRM_INTERVAL": "0",
+           "GH_LOG": str(log), "GH_EXISTING_TAGS": gh.pop("existing", ""), **gh}
+    done = subprocess.run(["bash", str(CLAIM), str(root)], capture_output=True, text=True, timeout=30, env=env)
+    return done, log.read_text().splitlines()
+
+
+def test_a_new_version_creates_the_tag_and_starts_the_release(tmp_path):
+    done, calls = claim(tmp_path, existing="v1.2.2 v1.2.30")
+    assert done.returncode == 0, done.stderr
+    assert calls[1] == "api repos/o/r/git/refs -f ref=refs/tags/v1.2.3 -f sha=abc123"
+    assert calls[2] == "workflow run release.yml --repo o/r --ref v1.2.3 -f tag=v1.2.3"
+    # The job confirms that the release run exists.
+    assert calls[3].startswith("api repos/o/r/actions/workflows/release.yml/runs?event=workflow_dispatch&head_sha=abc123")
+    assert len(calls) == 4
+
+
+def test_the_release_run_is_confirmed_after_the_start(tmp_path):
+    # The run appears at the third lookup: the job waits for it.
+    done, calls = claim(tmp_path / "late", GH_RUN_AT="3")
+    assert done.returncode == 0, done.stderr
+    assert sum("actions/workflows/release.yml/runs" in call for call in calls) == 3
+
+    # `gh workflow run` reported success, but no run exists: the job fails.
+    done, calls = claim(tmp_path / "never", GH_RUN_AT="0")
+    assert done.returncode != 0
+    assert sum("actions/workflows/release.yml/runs" in call for call in calls) == 3
+    assert "no release run" in done.stderr
+    assert "gh workflow run release.yml --ref v1.2.3 -f tag=v1.2.3" in done.stderr
+
+
+def test_an_existing_tag_releases_nothing(tmp_path):
+    done, calls = claim(tmp_path, existing="v1.2.3")
+    assert done.returncode == 0, done.stderr
+    assert len(calls) == 1 and "matching-refs" in calls[0], calls
+    assert "releases nothing" in done.stdout
+
+
+def test_a_lost_race_for_the_tag_releases_nothing(tmp_path):
+    done, calls = claim(tmp_path, GH_CREATE="exists")
+    assert done.returncode == 0, done.stderr
+    assert not any(call.startswith("workflow run") for call in calls), calls
+
+
+def test_api_failures_fail_the_job_and_start_no_release(tmp_path):
+    for index, failure in enumerate([{"GH_LOOKUP": "fail"}, {"GH_CREATE": "denied"}]):
+        done, calls = claim(tmp_path / str(index), **failure)
+        assert done.returncode != 0, failure
+        assert not any(call.startswith("workflow run") for call in calls), calls
+    # A failed start of the release fails the job, and the message names the recovery command.
+    done, _ = claim(tmp_path / "dispatch", GH_DISPATCH="fail")
+    assert done.returncode != 0
+    assert "gh workflow run release.yml --ref v1.2.3 -f tag=v1.2.3" in done.stderr
+
+
+def test_inconsistent_version_files_start_no_release(tmp_path):
+    root = make_repo(tmp_path / "repo", server="1.2.2")
+    done = subprocess.run(["bash", str(CLAIM), str(root)], capture_output=True, text=True, timeout=30,
+                          env={"PATH": "/usr/bin:/bin", "GITHUB_REPOSITORY": "o/r", "GITHUB_SHA": "abc123"})
+    assert done.returncode != 0
+    assert "server.json" in done.stderr

### New Points
R1. S1 applied: the Cargo test job of ci.yml runs `shellcheck .github/scripts/*.sh` (that job already installs shellcheck). Locally, shellcheck 0.11.0 has no finding on both scripts, and actionlint 1.7.12 with shellcheck integration has no finding on ci.yml and release.yml (the unknown-label finding for ubuntu-26.04 is ignored; it exists on master).
R2. S2 applied: the jq filters contain no shell data. The script exports WANT_TAG and WANT_REF and the filters compare with env.WANT_REF and env.WANT_TAG. Checked against the real API with gh 2.104.0: 1 exact match for v0.1.6, 0 for v0.1.7 under the prefix v0.1. release-version.sh and AGENTS.md state that build metadata ('+...') is not supported, with a test that rejects it.
R3. S4 applied: release-version.sh reads `version` from the [package] table only (awk tracks the current table). New test: a [workspace.package] table with another version before [package] does not change the result. That test failed before the change.
R4. S3 not applied, with a reason: a concurrency group on one job of ci.yml would make a later CI run on master wait for an earlier one only at that job, and with cancel-in-progress false GitHub keeps one pending run per group and cancels older pending ones, which can cancel the release job of the commit that has the new version. The tag creation is already an atomic claim, and a double dispatch needs two successful creations of one ref, which the API refuses. Two different versions in quick succession are two different tags and two intended releases.
R5. Verification of a519f78 on the worker host: tests/test_release_version.py has 12 tests and they pass. CI on a519f78 was running at the time of this rebuttal; CI on the first commit 90b8275 was replaced by it. The job release-on-merge itself has no run before the merge, because it runs only for a push to master. The merge of this PR runs its 'tag exists' path, since the version stays 0.1.6.

**Requesting:** RE-REVIEW

<!-- event id="review-2" reviewer.model="anthropic/claude-fable-5" reviewer.provider="openrouter" reviewer.validation_attempts="1" -->
## Review Response — Round 2
**Position:** AGREE

### Blocking Concerns
None.

### Non-Blocking Suggestions
None.

### Responses to Prior Points
Re B1: The fix is correct and complete. publish-registry now derives TAG identically to the release job (tag ref of the run, else the `tag` input), release-on-merge.sh passes both `--ref <tag>` and `-f tag=<tag>` so the tag reaches the run through both channels, and the recovery command in AGENTS.md and the script error message use the same form. The release.yml comment also now documents the branch-dispatch-with-input path. Both documented dispatch paths now resolve TAG correctly. Resolved.
Re B2: The empirical probe satisfies part (a) convincingly: a real run with the same token scopes (contents: write, actions: write) exercised the real script against the real API, demonstrating tag-ref creation, dispatch on a just-created tag, confirmation lookup finding the run (head_branch == tag name), and the idempotent second call. The probe's two limits (no -f tag input in the probe, non-v tag prefix) are acceptable; neither affects the token-capability question. Part (b), the MCP Registry OIDC login from a workflow_dispatch run, remains unverified, but I accept the worker's reasoning: it cannot be tested without a real publish, its failure mode is late and visible (last job of the Release run fails after the GitHub Release exists, re-runnable), and it is documented in the PR description and AGENTS.md. That residual risk is acceptable and does not block. Resolved.
Re B3: The post-dispatch confirmation loop is implemented as requested: it polls the runs API filtered by event, head_sha, and head_branch == tag (comparison via env.WANT_TAG, no interpolated data), fails loudly with the recovery command after the configured attempts, and the probe under B2 validated the filter against the real API (head_branch of a dispatch on a tag ref is the tag name). Tests cover the run appearing late (succeeds after 3 lookups), never appearing (fails with recovery command on stderr), and the happy path asserting the lookup call. A silently-dropped dispatch now fails the job. Resolved.
Re R1: S1 accepted and applied well: shellcheck runs on both scripts in the CI job that already installs it, and both shellcheck 0.11.0 and actionlint with shellcheck integration are clean locally. Satisfied.
Re R2: S2 applied exactly as suggested: jq filters now compare against env.WANT_REF/env.WANT_TAG with the values exported, so no shell data is interpolated into the filters, and the build-metadata limitation is documented in both the script and AGENTS.md with a rejecting test. Verified against the real API. Satisfied.
Re R3: S4 applied: the awk extraction reads version only from the [package] table, and the new test with a preceding [workspace.package] table failed before the change, which proves the test is load-bearing. Satisfied.
Re R4: I accept the rejection of S3. The point about GitHub's pending-run semantics is correct: with cancel-in-progress false, a concurrency group keeps only one pending run and cancels older pending ones, which could cancel exactly the run carrying the new version — worse than the race it would prevent. The tag creation is already an atomic claim, double dispatch requires two successful creations of one ref, and two different versions are two intended releases. The suggestion is withdrawn.
Re R5: Noted. 12 tests passing, CI running on a519f78, and the honest statement of the residual limitation (the job's release path has no real run before merge; the merge exercises the tag-exists path) are consistent with the probe evidence under B2, which exercised the release path against the real API. The verification picture is adequate for approval.
B1: resolved
B2: resolved
B3: resolved

**Verdict:** APPROVE
