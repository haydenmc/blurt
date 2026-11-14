#!/usr/bin/env bash
# Exports blurt's changes to crates/mumble-protocol as patches for Mumbo
# (https://github.com/citelao/mumbo), one per commit. See docs/upstream.md.
#
#   scripts/upstream-patches.sh [--verify] [OUT_DIR]
#
# --verify applies the patches to a fresh Mumbo clone at the synced commit
# and runs the crate's tests there.
set -euo pipefail

CRATE=crates/mumble-protocol
UPSTREAM_URL=${UPSTREAM_URL:-https://github.com/citelao/mumbo}

verify=false
if [[ ${1:-} == --verify ]]; then
    verify=true
    shift
fi
out=${1:-target/upstream-patches}

cd "$(git rev-parse --show-toplevel)"

# The last sync with upstream is the newest commit carrying an
# `Upstream-Sync: <mumbo commit>` trailer.
base=$(git log --format=%H --grep='^Upstream-Sync: ' -n 1)
if [[ -z $base ]]; then
    echo "No commit with an Upstream-Sync trailer found." >&2
    exit 1
fi
upstream=$(git log -1 --format=%B "$base" | sed -n 's/^Upstream-Sync: *//p' | tail -1)
echo "Synced with Mumbo $upstream at $(git log -1 --format='%h %s' "$base")"

rm -rf "$out"
mkdir -p "$out"
n=0
for commit in $(git rev-list --reverse --no-merges "$base"..HEAD -- "$CRATE"); do
    summary=$(git log -1 --format='%h %s' "$commit")
    if git log -1 --format=%B "$commit" | grep -qi '^Upstream: *skip'; then
        echo "  skip    $summary"
        continue
    fi
    others=$(git diff-tree --no-commit-id --name-only -r "$commit" |
        grep -v "^$CRATE/" | grep -v '^Cargo.lock$' || true)
    if [[ -n $others ]]; then
        echo "  warning: $summary also changes files outside the crate (left out):" >&2
        echo "$others" | sed 's/^/             /' >&2
    fi
    n=$((n + 1))
    git format-patch -q -1 --start-number "$n" -o "$out" "$commit" -- \
        "$CRATE" ":(exclude)$CRATE/NOTICE"
    echo "  export  $summary"
done
echo "Wrote $n patches to $out"

if $verify && ((n > 0)); then
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    echo "Verifying against a fresh clone of $UPSTREAM_URL at $upstream..."
    git clone -q "$UPSTREAM_URL" "$work/mumbo"
    git -C "$work/mumbo" switch -q --detach "$upstream"
    git -C "$work/mumbo" -c user.name=verify -c user.email=verify@localhost \
        am -q "$(realpath "$out")"/*.patch
    (cd "$work/mumbo" && cargo test -q -p mumble-protocol)
    echo "All patches apply to Mumbo $upstream and its mumble-protocol tests pass."
fi
