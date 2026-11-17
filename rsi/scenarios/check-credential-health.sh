#!/bin/sh
# Check: scripts/keep/credential-health.sh raises one owner alert per fault class (delivery
# missing after the boot grace, delivery not loaded, relay down, provider refusal, failed calls,
# legacy env file or unit wiring), never twice while open, clears it when the fault ends, and
# prints or records no credential value. Fake units, journal, health answer and alert store in a
# scratch folder; nothing live is read or changed. About 3 s. Exit 0 when every case passes.
set -eu
here=$(pwd)
for f in credential-health.sh test-credential-health.sh; do
    [ -f "$here/scripts/keep/$f" ] || { echo "check-credential-health: no scripts/keep/$f in the tree"; exit 1; }
done
if bash "$here/scripts/keep/test-credential-health.sh"; then
    echo "check-credential-health: ok"
else
    echo "check-credential-health: FAIL"; exit 1
fi
