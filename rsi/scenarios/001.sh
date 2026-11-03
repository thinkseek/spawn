#!/bin/sh
# Black box: `spawn version` prints one line that starts with "spawn ".
# Fails on the baseline, where the subcommand does not exist.
set -eu
output=$("$SPAWN_BIN" version)
[ "$(printf '%s\n' "$output" | grep -c '^spawn ')" -eq 1 ]
