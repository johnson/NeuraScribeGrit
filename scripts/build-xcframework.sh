#!/bin/bash
# Build the static library for every Apple slice and package them as an
# xcframework that the Swift package can consume.
#
# The output is a build artifact and is gitignored: the Rust source in this
# repository is the source of truth, and anyone can rebuild it with this script.
#
# Usage: scripts/build-xcframework.sh
set -euo pipefail

cd "$(dirname "$0")/.."

export CARGO_INCREMENTAL=0
# The target directory lives off the repo so ExFAT's lack of hardlinks does not
# slow the build down, and so a clean is a plain rm -rf.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/Volumes/Store2T/.nsg-target}"

OUT=neurascribe_gritFFI.xcframework
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

# name:rust-target
SLICES=(
  "ios-arm64:aarch64-apple-ios"
  "ios-arm64-simulator:aarch64-apple-ios-sim"
  "macos-arm64:aarch64-apple-darwin"
)

for entry in "${SLICES[@]}"; do
  name="${entry%%:*}"
  target="${entry##*:}"
  echo "==> building $name ($target)"
  cargo build --release --target "$target"

  lib="$CARGO_TARGET_DIR/$target/release/libneurascribe_grit_ffi.a"
  test -f "$lib" || { echo "missing $lib" >&2; exit 1; }
  mkdir -p "$STAGE/$name"
  cp "$lib" "$STAGE/$name/"
done

echo "==> packaging $OUT"
rm -rf "$OUT"
xcrun xcodebuild -create-xcframework \
  -library "$STAGE/ios-arm64/libneurascribe_grit_ffi.a" -headers include \
  -library "$STAGE/ios-arm64-simulator/libneurascribe_grit_ffi.a" -headers include \
  -library "$STAGE/macos-arm64/libneurascribe_grit_ffi.a" -headers include \
  -output "$OUT"

echo "==> done: $OUT"
du -sh "$OUT"
