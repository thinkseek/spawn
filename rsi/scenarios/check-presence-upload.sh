#!/bin/sh
# Check: the observe3 presence uploader (scripts/observe3/presence_upload.py) stays
# off by default, sends only the validated document to its fixed HTTPS URL, and
# never sends a test token on the production path. Runs its synthetic tests
# against a made-up receiver on a local port; nothing leaves the machine. About
# 9 s. Exit 0 when every case passes; 1 otherwise.
set -eu
# Only when the tree differs from the base (SPAWN_BASELINE_TREE) in what this
# check runs or reads: scripts/observe3/ (the uploader and its test) and its
# systemd units. Otherwise it passes at once, as check-test-gate.sh does, so a
# tree that leaves them alone pays nothing (it never caught a regression in the
# gate ROI audit of 2026-11-24). Its own file is on the list, so a change to the
# check runs it.
base=${SPAWN_BASELINE_TREE-}
if [ -n "$base" ] && [ -d "$base" ]; then
    same=1
    for path in scripts/observe3 deploy/systemd/spawn-agent-presence-upload.service \
        deploy/systemd/spawn-agent-presence-upload.timer rsi/scenarios/check-presence-upload.sh; do
        if [ -e "$path" ] || [ -e "$base/$path" ]; then
            diff -rq -- "$base/$path" "$path" >/dev/null 2>&1 || {
                same=""
                echo "changed against the base: $path"
            }
        fi
    done
    if [ -n "$same" ]; then
        echo "check-presence-upload: the uploader and its test are the base's: not run"
        exit 0
    fi
fi
here=$(pwd)
for f in presence_upload.py test_presence_upload.py agent_presence.py; do
    [ -f "$here/scripts/observe3/$f" ] || { echo "check-presence-upload: no scripts/observe3/$f in the tree"; exit 1; }
done
out=$(mktemp)
trap 'rm -f "$out"' EXIT
rc=0
python3 -I "$here/scripts/observe3/test_presence_upload.py" >"$out" 2>&1 || rc=$?
tail -4 "$out"
if [ "$rc" -eq 0 ] && grep -q '^OK$' "$out"; then
    echo "check-presence-upload: ok"
else
    echo "check-presence-upload: FAIL (exit $rc)"; exit 1
fi
