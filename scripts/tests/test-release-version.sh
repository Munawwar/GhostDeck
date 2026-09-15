#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
VALIDATOR="$ROOT_DIR/scripts/validate-release-version.sh"
WORKSPACE_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"

actual="$($VALIDATOR "$WORKSPACE_VERSION")"
[ "$actual" = "$WORKSPACE_VERSION" ]

for invalid in "" "v$WORKSPACE_VERSION" "0.1" "next"; do
    if "$VALIDATOR" "$invalid" >/dev/null 2>&1; then
        echo "FAIL: accepted invalid release version: ${invalid:-<empty>}" >&2
        exit 1
    fi
done

mismatch="999.999.999"
if "$VALIDATOR" "$mismatch" >/dev/null 2>&1; then
    echo "FAIL: accepted version that differs from Cargo.toml: $mismatch" >&2
    exit 1
fi

echo "release version validation: OK"
