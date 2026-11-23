#!/bin/sh
# Check: the observe3 agent-presence exporter (scripts/observe3/agent_presence.py)
# stays sanitized, read-only and bounded. Runs its synthetic tests against a
# made-up page service on a local port inside the test; nothing on the host is
# read or changed. About 9 s. Exit 0 when every case passes; 1 otherwise.
set -eu
# Only when the tree differs from the base (SPAWN_BASELINE_TREE) in what this
# check runs or reads: scripts/observe3/ (the exporter and its test) and its
# systemd units. Otherwise it passes at once, as check-test-gate.sh does, so a
# tree that leaves them alone pays nothing (it never caught a regression in the
# gate ROI audit of 2026-11-24). Its own file is on the list, so a change to the
# check runs it.
base=${SPAWN_BASELINE_TREE-}
if [ -n "$base" ] && [ -d "$base" ]; then
    same=1
    for path in scripts/observe3 deploy/systemd/spawn-agent-presence.service deploy/systemd/spawn-agent-presence.timer \
        rsi/scenarios/check-agent-presence.sh; do
        if [ -e "$path" ] || [ -e "$base/$path" ]; then
            diff -rq -- "$base/$path" "$path" >/dev/null 2>&1 || {
                same=""
                echo "changed against the base: $path"
            }
        fi
    done
    if [ -n "$same" ]; then
        echo "check-agent-presence: the exporter and its test are the base's: not run"
        exit 0
    fi
fi
here=$(pwd)
[ -f "$here/scripts/observe3/agent_presence.py" ] || { echo "check-agent-presence: no scripts/observe3/agent_presence.py in the tree"; exit 1; }
[ -f "$here/scripts/observe3/test_agent_presence.py" ] || { echo "check-agent-presence: no scripts/observe3/test_agent_presence.py in the tree"; exit 1; }
out=$(mktemp)
trap 'rm -f "$out"' EXIT
rc=0
python3 -I "$here/scripts/observe3/test_agent_presence.py" >"$out" 2>&1 || rc=$?
tail -4 "$out"
# The exit status is the tests' own, never a pipe's last command.
if [ "$rc" -eq 0 ] && grep -q '^OK$' "$out"; then
    echo "check-agent-presence: ok"
else
    echo "check-agent-presence: FAIL (exit $rc)"; exit 1
fi
