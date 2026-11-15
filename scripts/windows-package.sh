#!/usr/bin/env bash
# Builds blurt for Windows on Linux and packages it:
#   dist/blurt-x86_64-windows.zip        the program, to run from anywhere
#   dist/blurt-x86_64-windows-setup.exe  a per-user installer (NSIS)
# Cross-compiles with cargo-xwin, which downloads Microsoft's CRT and SDK
# (accepting their licence) into XWIN_CACHE_DIR. Needs the tools in
# packaging/windows/Containerfile; scripts/windows-build.sh runs this there.
set -euo pipefail
cd "$(dirname "$0")/.."

target=x86_64-pc-windows-msvc
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target/windows}
export XWIN_ARCH=${XWIN_ARCH:-x86_64}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
work=$CARGO_TARGET_DIR/package
stage=$work/blurt
rm -rf "$work" dist/blurt-x86_64-windows*
mkdir -p "$stage" dist

# The app icon, at the sizes Windows uses; the largest stays PNG.
svg=packaging/icons/io.github.haydenmc.blurt.svg
for size in 16 24 32 48 64 256; do
  rsvg-convert -w "$size" -h "$size" "$svg" -o "$work/icon-$size.png"
done
icon=$PWD/$work/blurt.ico
icotool -c -o "$icon" "$work"/icon-{16,24,32,48,64}.png --raw="$work/icon-256.png"

# build.rs embeds the icon with llvm-rc; distros may only ship it versioned.
if [ -z "${RC:-}" ] && ! command -v llvm-rc >/dev/null; then
  RC=$(ls /usr/lib/llvm-*/bin/llvm-rc 2>/dev/null | sort -V | tail -1 || true)
  export RC
fi

# PipeWire is Linux-only; cpal uses WASAPI on Windows.
BLURT_WINDOWS_ICON=$icon cargo xwin build --release --locked -p blurt \
  --target "$target" --no-default-features --features gpu

exe=$CARGO_TARGET_DIR/$target/release/blurt.exe
size=$(stat -c %s "$exe")
echo "blurt.exe is $((size / 1024)) KiB"
if [ "$size" -gt $((21 * 1000 * 1000)) ]; then
  echo "blurt.exe is over the 21 MB budget" >&2
  exit 1
fi

cp "$exe" README.md "$stage"/
sed 's/$/\r/' LICENSE > "$stage/LICENSE.txt"
(cd "$work" && zip -q -r -9 "$OLDPWD/dist/blurt-x86_64-windows.zip" blurt)

makensis -V2 \
  -DVERSION="$version" \
  -DVERSION_QUAD="${version%%-*}.0" \
  -DSTAGE="$PWD/$stage" \
  -DICON="$icon" \
  -DOUTFILE="$PWD/dist/blurt-x86_64-windows-setup.exe" \
  packaging/windows/blurt.nsi

ls -l dist/blurt-x86_64-windows*
