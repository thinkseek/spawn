#!/bin/sh
# Check: cargo test on the whole workspace, database tests included.
#
# On a failure it prints one line per failing test, `FAILED <test>: <first
# message line>`, before the usual excerpt, so that every failure is on
# record even when there are many (a database crash fails them all at once,
# and the integrator tells a host fault from a regression by reading them).
set -eu
: "${SPAWN_TEST_DATABASE_URL:?the database tests need SPAWN_TEST_DATABASE_URL; they do not skip}"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
if ! cargo test --quiet --workspace >"$log" 2>&1; then
    awk '
        function flush() { if (name != "") printf "FAILED %s: %s%s\n", name, at, msg; name = ""; at = ""; msg = "" }
        /^---- .* stdout ----$/ { flush(); name = $2; next }
        /^(failures:|test result:)/ { flush(); next }
        name != "" && at == "" && /panicked at/ { sub(/^thread .* panicked at /, ""); at = $0 " "; next }
        name != "" && msg == "" && NF { msg = substr($0, 1, 200) }
        END { flush() }
    ' "$log"
    grep -E 'FAILED|panicked|^error' -A5 "$log" | head -n 40
    exit 1
fi
