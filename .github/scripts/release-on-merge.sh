#!/usr/bin/env bash
# Release on merge (Vikunja #1622). Run by ci.yml after a push to master, when
# all CI jobs passed.
#
# Rule: release `v<version>` when no tag exists for the version in Cargo.toml.
# So a merge that changes the version releases, and a merge that keeps the
# version releases nothing.
#
# Steps: create the tag at the tested commit, then start release.yml on the
# tag ref. That is the same run as a manual release from a pushed tag. A tag
# that the workflow token creates does not start release.yml by itself, so
# this script starts it.
#
# Usage: release-on-merge.sh [REPOSITORY_ROOT]
# Environment: GITHUB_REPOSITORY, GITHUB_SHA, and a token for `gh` with
# `contents: write` and `actions: write`. Optional: RELEASE_CONFIRM_ATTEMPTS
# (default 12) and RELEASE_CONFIRM_INTERVAL in seconds (default 5).
set -euo pipefail

root="${1:-.}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

version=$("$here/release-version.sh" "$root")
tag="v$version"

# The endpoint matches by prefix (v0.1.7 also returns v0.1.70), so count the
# exact matches. An API failure stops the script: it is not "no tag". The jq
# filters read the tag from the environment, so they contain no shell data.
export WANT_TAG="$tag" WANT_REF="refs/tags/$tag"
existing=$(gh api "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/$tag" \
    --jq '[.[] | select(.ref == env.WANT_REF)] | length')
if [[ "$existing" != "0" ]]; then
    echo "Tag $tag exists. This merge releases nothing."
    exit 0
fi

# Creating the ref is the claim: of two runs, only one succeeds.
if ! created=$(gh api "repos/$GITHUB_REPOSITORY/git/refs" \
    -f "ref=refs/tags/$tag" -f "sha=$GITHUB_SHA" 2>&1); then
    if grep -q "Reference already exists" <<< "$created"; then
        echo "Another run created $tag. This merge releases nothing."
        exit 0
    fi
    echo "release-on-merge: could not create $tag: $created" >&2
    exit 1
fi
echo "Created $tag at $GITHUB_SHA."

not_started() {
    echo "release-on-merge: $tag exists, but $1." >&2
    echo "Later merges do not try again. Start it with: gh workflow run release.yml --ref $tag -f tag=$tag" >&2
    exit 1
}

# The run starts on the tag ref and also gets the tag as its input, so
# release.yml finds the tag in both of the places that it reads.
gh workflow run release.yml --repo "$GITHUB_REPOSITORY" --ref "$tag" -f "tag=$tag" ||
    not_started "the release did not start"

# `gh workflow run` reports success when GitHub accepts the request. That does
# not show that a run exists. Wait for the run, so that a dropped request fails
# this job and is not a tag without a release behind a green check.
attempts="${RELEASE_CONFIRM_ATTEMPTS:-12}"
interval="${RELEASE_CONFIRM_INTERVAL:-5}"
for ((attempt = 1; attempt <= attempts; attempt++)); do
    runs=$(gh api "repos/$GITHUB_REPOSITORY/actions/workflows/release.yml/runs?event=workflow_dispatch&head_sha=$GITHUB_SHA&per_page=50" \
        --jq '[.workflow_runs[] | select(.head_branch == env.WANT_TAG)] | length')
    if [[ "$runs" != "0" ]]; then
        echo "Started release.yml for $tag."
        exit 0
    fi
    if ((attempt < attempts)); then
        sleep "$interval"
    fi
done
not_started "no release run exists after $attempts lookups"
