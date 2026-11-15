#!/bin/sh
# Check: the passive shadow observer (scripts/shadow/shadow_observe.py) stays
# read-only and bounded. Runs its synthetic tests: fake commands in a scratch
# folder; nothing on the host is probed, stopped or changed. About 10 s.
# Exit 0 when every case passes; 1 otherwise, or when the files are missing.
set -eu
here=$(pwd)
[ -f "$here/scripts/shadow/shadow_observe.py" ] || { echo "check-shadow-observe: no scripts/shadow/shadow_observe.py in the tree"; exit 1; }
[ -f "$here/scripts/shadow/test_shadow_observe.py" ] || { echo "check-shadow-observe: no scripts/shadow/test_shadow_observe.py in the tree"; exit 1; }
out=$(mktemp)
trap 'rm -f "$out"' EXIT
rc=0
python3 -I "$here/scripts/shadow/test_shadow_observe.py" >"$out" 2>&1 || rc=$?
tail -4 "$out"
# The exit status is the tests' own, never a pipe's last command.
if [ "$rc" -eq 0 ] && grep -q '^OK$' "$out"; then
    echo "check-shadow-observe: ok"
else
    echo "check-shadow-observe: FAIL (exit $rc)"; exit 1
fi
