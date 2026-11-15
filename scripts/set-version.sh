#!/usr/bin/env bash
# Stamps a release version into the workspace, from a tag: v0.4.0 -> 0.4.0.
# In git the workspace stays at 0.0.0; release builds run this first.
# Edits Cargo.lock too, so --locked and offline (Flatpak) builds still work.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")/.."
version=${1:?usage: scripts/set-version.sh <tag or version>}
version=${version#v}
if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "Not a version: $1" >&2
  exit 1
fi

sed -i '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "'"$version"'"/' Cargo.toml
awk -v v="$version" '
  /^name = / { name = $3 }
  /^version = / && name ~ /^"(blurt|blurt-audio|blurt-cli|blurt-platform)"$/ {
    $0 = "version = \"" v "\""; name = ""
  }
  { print }
' Cargo.lock > Cargo.lock.new
mv Cargo.lock.new Cargo.lock

# Not cargo: it would need every crate downloaded, and the --locked builds
# that follow check the lock anyway.
stamped=$(grep -A1 -E '^name = "(blurt|blurt-audio|blurt-cli|blurt-platform)"$' Cargo.lock |
  grep -cxF "version = \"$version\"" || true)
if [ "$stamped" -ne 4 ]; then
  echo "Expected 4 workspace crates in Cargo.lock at $version, found $stamped" >&2
  exit 1
fi
echo "Version set to $version"
