#!/bin/sh
# Black box (objective 146) [recoverability]: the session backups' script and
# its test live in the tree (deploy/session-backup.sh, byte for byte the copy
# installed by hand in /usr/local/lib/spawn), and the tree's test passes.
#
# The tree under test is the working directory (as in 107.sh and 139.sh).
# From it, this runs `bash deploy/test-session-backup.sh` under a scratch HOME
# and TMPDIR of this run's own. That test runs session-backup.sh on scratch
# homes and a scratch backup root only, and its guard refuses the real home.
# It must exit 0 with its last line "<N> ok, 0 failed", with N at least 68 (the
# installed version's count), so a test emptied of its checks does not pass.
# On a tree without deploy/session-backup.sh or its test, the check fails.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "146: harness: $*" >&2
    exit 2
}
check_failed() {
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

for tool in bash mktemp getent; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
tree=$(pwd)
[ -d "$tree/deploy" ] || harness "run from the tree under test: no deploy/ under $tree"

WORK=$(mktemp -d) || harness "mktemp failed"
finish() {
    rc=$?
    trap - EXIT
    set +e
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "146: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
[ -n "$real_home" ] || harness "no home for uid $(id -u)"
case $WORK/ in "$real_home"/*) harness "the scratch $WORK is in the real home $real_home" ;; esac
mkdir -m 700 "$WORK/home" "$WORK/tmp" || harness "cannot make the scratch HOME and TMPDIR"

[ -f "$tree/deploy/session-backup.sh" ] ||
    check_failed "the tree has no deploy/session-backup.sh (the session backups' script is not in the tree)"
[ -f "$tree/deploy/test-session-backup.sh" ] ||
    check_failed "the tree has no deploy/test-session-backup.sh"

status=0
HOME=$WORK/home TMPDIR=$WORK/tmp bash "$tree/deploy/test-session-backup.sh" >"$WORK/test.out" 2>&1 </dev/null || status=$?
last=$(tail -n 1 "$WORK/test.out")
failing=$(grep -m 5 '^FAIL' "$WORK/test.out" | cut -c1-200 | tr '\n' '|')
[ "$status" -eq 0 ] || check_failed "deploy/test-session-backup.sh exited $status; its last line: $last; $failing"
passed=$(printf '%s\n' "$last" | sed -n 's/^\([0-9][0-9]*\) ok, 0 failed$/\1/p')
[ -n "$passed" ] || check_failed "deploy/test-session-backup.sh's last line is not '<N> ok, 0 failed': $last"
[ "$passed" -ge 68 ] || check_failed "deploy/test-session-backup.sh ran $passed checks, fewer than the installed version's 68"
echo "146: the tree's session-backup test passes: $last"
exit 0
