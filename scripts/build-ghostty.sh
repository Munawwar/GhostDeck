#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TEMP_DIR="$(mktemp -d -t ghostdeck-ghostty-XXXXXX)"
SOURCE_DIR="$TEMP_DIR/source"

trap 'git -C "$ROOT_DIR/ghostty" worktree remove --force "$SOURCE_DIR" >/dev/null 2>&1 || true; rm -rf "$TEMP_DIR"' EXIT

git -C "$ROOT_DIR/ghostty" worktree add --detach "$SOURCE_DIR" HEAD >/dev/null
git -C "$SOURCE_DIR" apply "$ROOT_DIR/patches/ghostty-linux-embedded.patch"

(cd "$SOURCE_DIR" && zig build "$@")

if [ -f "$SOURCE_DIR/zig-out/lib/libghostty.so" ]; then
    mkdir -p "$ROOT_DIR/ghostty/zig-out/lib"
    # Replace via rename: copying over the file in place would corrupt a
    # running GhostDeck that has the old library mapped.
    cp "$SOURCE_DIR/zig-out/lib/libghostty.so" "$ROOT_DIR/ghostty/zig-out/lib/libghostty.so.tmp"
    mv -f "$ROOT_DIR/ghostty/zig-out/lib/libghostty.so.tmp" "$ROOT_DIR/ghostty/zig-out/lib/libghostty.so"
fi
if [ -d "$SOURCE_DIR/zig-out/share" ]; then
    mkdir -p "$ROOT_DIR/ghostty/zig-out/share"
    cp -a "$SOURCE_DIR/zig-out/share/." "$ROOT_DIR/ghostty/zig-out/share/"
fi
