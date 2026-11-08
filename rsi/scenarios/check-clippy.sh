#!/bin/sh
# Check: cargo clippy on the whole workspace, with no warning.
set -eu
log=$(mktemp)
trap 'rm -f "$log"' EXIT
if ! cargo clippy --quiet --workspace --all-targets >"$log" 2>&1; then
    tail -n 30 "$log"
    exit 1
fi
if grep -qE '^(warning|error)' "$log"; then
    grep -E '^(warning|error)' -A4 "$log" | head -n 30
    exit 1
fi
