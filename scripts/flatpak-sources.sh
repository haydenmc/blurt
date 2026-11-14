#!/usr/bin/env bash
# Writes packaging/flatpak/cargo-sources.json from Cargo.lock, so the Flatpak
# build can fetch every crate offline. Needs python3 with aiohttp and toml
# (Debian: python3-aiohttp python3-toml).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
# flatpak-builder-tools, pinned.
TOOLS_COMMIT=9d86d336a39b9af38b86f114b6ce5820fe260f91
generator=$(mktemp -d)/flatpak-cargo-generator.py
curl -fsSL -o "$generator" \
  "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$TOOLS_COMMIT/cargo/flatpak-cargo-generator.py"
python3 "$generator" Cargo.lock -o packaging/flatpak/cargo-sources.json
