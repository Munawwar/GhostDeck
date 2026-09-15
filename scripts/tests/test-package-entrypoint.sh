#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../.." && pwd)"

# Exercise the production assertion without building or installing packages.
# shellcheck disable=SC1090
source <(sed -n '/^assert_cli_entrypoint() {$/,/^}$/p' "$ROOT_DIR/scripts/package.sh")

large_help() {
    printf 'limux CLI\n'
    # More than a pipe buffer: an early-closing grep must not kill the producer.
    printf '%1048576s\n' ''
}

failed_help() {
    printf 'limux CLI\n'
    return 42
}

host_help() {
    printf 'GApplication options\n'
}

if ! (assert_cli_entrypoint large_help "large CLI help"); then
    echo "FAIL: rejected successful CLI help larger than a pipe buffer" >&2
    exit 1
fi

if (assert_cli_entrypoint failed_help "failed CLI help") >/dev/null 2>&1; then
    echo "FAIL: accepted a failing help command because its output matched" >&2
    exit 1
fi

if (assert_cli_entrypoint host_help "host help") >/dev/null 2>&1; then
    echo "FAIL: accepted a non-CLI entrypoint" >&2
    exit 1
fi

echo "package CLI entrypoint validation: OK"
