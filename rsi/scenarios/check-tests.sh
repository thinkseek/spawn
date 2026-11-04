#!/bin/sh
# Check: cargo test on the whole workspace, database tests included.
set -eu
: "${SPAWN_TEST_DATABASE_URL:?the database tests need SPAWN_TEST_DATABASE_URL; they do not skip}"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
if ! cargo test --quiet --workspace >"$log" 2>&1; then
    grep -E 'FAILED|panicked|^error' -A5 "$log" | head -n 40
    exit 1
fi
