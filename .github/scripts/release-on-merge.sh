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
# `contents: write` and `actions: write`.
set -euo pipefail

root="${1:-.}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

version=$("$here/release-version.sh" "$root")
tag="v$version"

# The endpoint matches by prefix (v0.1.7 also returns v0.1.70), so count the
# exact matches. An API failure stops the script: it is not "no tag".
existing=$(gh api "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/$tag" \
    --jq "[.[] | select(.ref == \"refs/tags/$tag\")] | length")
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

if ! gh workflow run release.yml --repo "$GITHUB_REPOSITORY" --ref "$tag"; then
    echo "release-on-merge: $tag exists, but the release did not start." >&2
    echo "Later merges do not try again. Start it with: gh workflow run release.yml --ref $tag" >&2
    exit 1
fi
echo "Started release.yml for $tag."
