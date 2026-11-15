#!/usr/bin/env bash
# Runs a command in the Windows cross-build container
# (packaging/windows/Containerfile): by default scripts/windows-package.sh,
# so building on your machine and in CI is the same. Needs podman or docker.
#
#   scripts/windows-build.sh                 # zip + installer in dist/
#   scripts/windows-build.sh cargo xwin check --target x86_64-pc-windows-msvc ...
#
# BLURT_WINDOWS_IMAGE uses a ready image instead of building one (CI does).
# Caches (crates, Microsoft's CRT and SDK) live in BLURT_WINDOWS_CACHE.
set -euo pipefail
cd "$(dirname "$0")/.."

engine=${CONTAINER_ENGINE:-$(command -v podman || command -v docker || true)}
if [ -z "$engine" ]; then
  echo "Needs podman or docker" >&2
  exit 1
fi

image=${BLURT_WINDOWS_IMAGE:-}
if [ -z "$image" ]; then
  image=blurt-windows-build
  "$engine" build -t "$image" -f packaging/windows/Containerfile packaging/windows
fi

cache=${BLURT_WINDOWS_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/blurt-windows}
mkdir -p "$cache"/{cargo,xwin,home}

# Run as you, so what it writes (dist/, target/windows) is yours. Rootless
# podman needs keep-id for that; rootful podman and docker map ids as-is.
user=(--user "$(id -u):$(id -g)")
if [ "$(basename "$engine")" = podman ] &&
  [ "$("$engine" info --format '{{.Host.Security.Rootless}}' 2>/dev/null)" = true ]; then
  user+=(--userns=keep-id)
fi

if [ $# -eq 0 ]; then
  set -- scripts/windows-package.sh
fi

"$engine" run --rm "${user[@]}" \
  --security-opt label=disable \
  -v "$PWD:/src" -w /src \
  -v "$cache:/cache" \
  -e CARGO_HOME=/cache/cargo \
  -e XWIN_CACHE_DIR=/cache/xwin \
  -e HOME=/cache/home \
  -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" \
  -e RUSTFLAGS="${RUSTFLAGS:-}" \
  "$image" "$@"
