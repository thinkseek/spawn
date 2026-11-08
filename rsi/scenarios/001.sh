#!/bin/sh
# Black box: `spawn version` prints one line that starts with "spawn ".
# Fails on the baseline, where the subcommand does not exist.
#
# Exit codes (scripts/rsi/gate.sh): 0 pass, 1 the assertion failed, 2 a
# harness error (no binary to run).
set -eu
[ -x "${SPAWN_BIN:-}" ] || {
    echo "001: harness: SPAWN_BIN is not an executable" >&2
    exit 2
}
output=$("$SPAWN_BIN" version) || exit 1
[ "$(printf '%s\n' "$output" | grep -c '^spawn ')" -eq 1 ] || exit 1
