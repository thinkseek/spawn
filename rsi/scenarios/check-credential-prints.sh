#!/bin/sh
# Check: no script of the tree prints a credential (objective 174, D8).
# scripts/rsi/check-credential-prints.py existed with no caller. No gate,
# scenario, check or precheck ran it (lead-mac, 2026-11-15), so a script that
# cats a credential file or echoes $SPAWN_DATABASE_URL could land.
#
# What it runs, in every gate, on every tree (about 10 s on this host):
# - the checker over scripts/, deploy/ and rsi/: every shell script there.
#   Any finding fails the check, named path:line. A line its author
#   accepted with `# credential-print: ok <why>` stays accepted, as the
#   checker counts it.
# - then scripts/rsi/test-check-credential-prints.sh, the checker's own
#   cases (about 3 s). They run against a made-up collector, so they hold
#   wherever the host's collector is. The checker is in scripts/rsi, so a
#   tree that weakens it fails here.
#
# The list of credentials is the value collector's, by name only. Those are
# its file lists and its rule for a credential's name, never a value. It is
# read from /usr/local/lib/spawn/redact/secret-values.py (world-readable,
# under the sandbox's read-only /), or from $SPAWN_SECRET_VALUES. The checker
# reads no credential file. With no collector, or one it cannot read, the
# checker refuses (exit 2), and so does this check (exit 1, one line naming
# the collector). It never passes with no list: a refusal on the candidate
# and on the base is a host fault in the gate, never a pass.
#
# Exit 0 clean. Exit 1 on a finding, a failing case, or a refusal.
set -eu
here=$(pwd)
check_py=$here/scripts/rsi/check-credential-prints.py
tests=$here/scripts/rsi/test-check-credential-prints.sh
[ -f "$check_py" ] || { echo "check-credential-prints: no scripts/rsi/check-credential-prints.py in the tree"; exit 1; }
[ -f "$tests" ] || { echo "check-credential-prints: no scripts/rsi/test-check-credential-prints.sh in the tree"; exit 1; }
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
roots=""
for root in scripts deploy rsi; do
    [ -d "$root" ] && roots="$roots $here/$root"
done
rc=0
# shellcheck disable=SC2086 # the roots are this tree's own directory names
python3 -I "$check_py" $roots >"$scratch/findings" 2>"$scratch/err" || rc=$?
case $rc in
0) ;;
1)
    # One line per finding: path:line, the command and the credential's file
    # or name, never the script's line itself.
    sed "s#^$here/##" "$scratch/findings" | head -n 40
    tail -n 1 "$scratch/err"
    exit 1
    ;;
*)
    echo "check-credential-prints: the checker refused (exit $rc): $(grep -m 1 refused "$scratch/err" || tail -n 1 "$scratch/err")"
    exit 1
    ;;
esac
# Then the checker's own cases: a tree that weakens the checker fails here.
if ! bash "$tests" >"$scratch/tests.out" 2>&1; then
    grep -E 'FAIL|refused' "$scratch/tests.out" | head -n 20
    tail -n 1 "$scratch/tests.out"
    exit 1
fi
tail -n 1 "$scratch/err"
tail -n 1 "$scratch/tests.out"
