#!/bin/sh
# Check (issue #98, PR-4): the tree's deploy/test-sync-units.sh passes on its
# own deploy/sync-units.sh, the step of every deploy (and of the cutover)
# that keeps the host's installed systemd user units the same as the
# deployed commit's deploy/systemd. Before it no deploy installed a unit,
# and spawn-maintenance.service ran two weeks with a setting main had
# removed. The test runs on a scratch unit directory and a fake systemctl
# of its own: no live unit is read or changed. About 1 s.
#
# Exit status: 0 every case passes; 1 one failed, or the tree has no test.
set -eu
for f in deploy/sync-units.sh deploy/test-sync-units.sh deploy/systemd/host-local; do
    [ -f "$f" ] || { echo "check-sync-units: no $f in the tree"; exit 1; }
done
if bash deploy/test-sync-units.sh; then
    echo "check-sync-units: ok"
else
    echo "check-sync-units: FAIL"; exit 1
fi
