#!/bin/sh
# Check: the observe3 agent-presence exporter (scripts/observe3/agent_presence.py)
# stays sanitized, read-only and bounded. Runs its synthetic tests against a
# made-up page service on a local port inside the test; nothing on the host is
# read or changed. About 9 s. Exit 0 when every case passes; 1 otherwise.
set -eu
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
