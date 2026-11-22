#!/usr/bin/env bash
# Print the version that a merge to master releases: the package version in
# Cargo.toml. Fail when Cargo.lock, server.json, or CHANGELOG.md names another
# version, so a release never ships a binary that reports a different version.
#
# Usage: release-version.sh [REPOSITORY_ROOT]
set -euo pipefail

root="${1:-.}"

fail() {
    echo "release-version: $1" >&2
    exit 1
}

# The `version = "..."` line of the [package] table, not of another table.
version=$(awk '
    /^\[/ { in_package = ($0 == "[package]") }
    in_package && /^version = ".*"$/ { sub(/^version = "/, ""); sub(/"$/, ""); print; exit }
' "$root/Cargo.toml")
# MAJOR.MINOR.PATCH with an optional prerelease part. Build metadata ("+...")
# is not supported: the version becomes a tag name and a part of API paths.
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    fail "Cargo.toml: the package version '$version' is not MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-PRERELEASE"
fi

lock=$(grep -A 1 '^name = "daimonos"$' "$root/Cargo.lock" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
if [[ "$lock" != "$version" ]]; then
    fail "Cargo.lock: the daimonos package has version '$lock', Cargo.toml has '$version'"
fi

server=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' "$root/server.json")
if [[ "$server" != "$version" ]]; then
    fail "server.json: version is '$server', Cargo.toml has '$version'"
fi

if ! grep -q -F "## [$version]" "$root/CHANGELOG.md"; then
    fail "CHANGELOG.md: no '## [$version]' section"
fi

echo "$version"
