#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REQUESTED_VERSION="${1:-}"
WORKSPACE_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -1)"

if [[ ! "$REQUESTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "ERROR: release version must use x.y.z without a v prefix: ${REQUESTED_VERSION:-<empty>}" >&2
    exit 1
fi

if [ "$REQUESTED_VERSION" != "$WORKSPACE_VERSION" ]; then
    echo "ERROR: release version $REQUESTED_VERSION does not match Cargo workspace version $WORKSPACE_VERSION" >&2
    exit 1
fi

printf '%s\n' "$REQUESTED_VERSION"
