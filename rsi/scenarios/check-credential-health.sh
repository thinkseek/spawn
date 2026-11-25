#!/bin/sh
# Check: scripts/keep/credential-health.sh raises one owner alert per row of
# scripts/keep/credentials.manifest and fault class (delivery missing after the boot grace,
# delivery not loaded, relay down, provider refusal, failed calls, legacy env file or unit wiring
# in any spawn-* unit or drop-in, a scanner source gone with its Keep twin; opt-in, unit drift),
# never twice while open, clears it when the fault ends, and prints or records no credential
# value. Then the value collector's Keep-first cases (scripts/redact/test-secret-values.sh: Keep's
# file alone, both, the plaintext alone, both gone fails closed) and its agreement with the
# manifest. Fake units, journal, health answer, alert store and credential files in scratch
# folders, made-up values only; nothing live is read or changed. About 5 s. Exit 0 when every
# case passes.
set -eu
here=$(pwd)
for f in keep/credential-health.sh keep/credentials.manifest keep/test-credential-health.sh \
    redact/secret-values.py redact/test-secret-values.sh; do
    [ -f "$here/scripts/$f" ] || { echo "check-credential-health: no scripts/$f in the tree"; exit 1; }
done
if bash "$here/scripts/keep/test-credential-health.sh" && bash "$here/scripts/redact/test-secret-values.sh"; then
    echo "check-credential-health: ok"
else
    echo "check-credential-health: FAIL"; exit 1
fi
